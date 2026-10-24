//! Where a frame's time went.
//!
//! A fixed set of named buckets a game author already has words for, plus
//! per-script attribution. Not a profiler: no sampling, no flamegraph, no call
//! stacks — the question is "which of my scripts is doing this", not "what is
//! the hot function".
//!
//! Every bucket keeps two numbers, a rolling mean and the worst of the last N
//! frames, because the spike is what anybody is chasing and a mean hides it: a
//! 40 ms hitch once a second adds under a millisecond to a 60-frame average.
//!
//! Collection costs nothing while nothing is looking, and "off" does not read
//! as "fast": [`FrameProfile::bucket`] returns `None` while disabled rather than
//! zero, so a budget assertion in a smoke test cannot pass by accident.

use std::collections::HashMap;

/// The named buckets a frame is divided into.
///
/// Fixed on purpose. These are the words a game author uses about their own
/// project, and a closed set means the editor's readout and the Lua surface
/// cannot disagree about what exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bucket {
    /// Lua: the whole of every pass (`update`, `fixedUpdate`, `lateUpdate`) —
    /// the per-instance setup and the hook calls alike, net of the scene mirror,
    /// which has its own bucket. [`FrameProfile::scripts`] breaks the hook time
    /// down per script; that breakdown is a part of this, not all of it, and
    /// the difference is what the engine spends getting to a hook.
    Scripts,
    /// The scene mirror: the ECS → Lua sync every script pass runs before it
    /// calls anything. Nested inside a script pass, and subtracted from
    /// [`Bucket::Scripts`] so the two do not count it twice.
    Mirror,
    /// The rigid-body sim, including collision and the character controllers.
    Physics,
    /// Terrain residency, field generation and surface-net meshing.
    Terrain,
    /// Scatter: chunk sweeps, settling raycasts, instance building.
    Scatter,
    /// Particle simulation and the batches it produces.
    Particles,
    /// The audio mixer: voice updates, spatialisation and the effect chain.
    Audio,
    /// Skeletal animation: clip sampling, blending, pose composition, skinning.
    Animation,
    /// Model imports on the main thread: what a model given to a node or
    /// spawned with a prefab costs the frame. Reading and decoding run on a
    /// worker; the upload lands here, and so does any import that could not
    /// wait (opening a scene).
    Models,
    /// Game UI: layout solve, hit-testing, the element draw list.
    Ui,
    /// Everything from the gather to submit: instances, the raymarch, the raster
    /// passes, post.
    Render,
    /// Textures, shaders and materials the frame needs on the GPU for the first
    /// time: decoding and uploading an image, compiling a shader.
    Assets,
    /// Waiting on the GPU: for a swapchain image to come free, which is the GPU
    /// still drawing an earlier frame (or, with vsync on, the display not yet
    /// wanting one), and for any readback the frame blocks on.
    GpuWait,
    /// Handing the finished image to the compositor.
    Present,
    /// The rest of the frame: the time between two frames that no other bucket
    /// measured — engine work nothing times yet, and the OS not running the
    /// game at all. Without it a slow frame whose buckets added up to little
    /// was not a hitch at all, and a frame that was mostly unmeasured named
    /// nothing. Named `untimed`; scripts written when it was `other` still get
    /// it by that name.
    Other,
}

impl Bucket {
    /// Every bucket, in the order a readout should list them — roughly the order
    /// of a frame.
    pub const ALL: [Bucket; 15] = [
        Bucket::Scripts,
        Bucket::Mirror,
        Bucket::Physics,
        Bucket::Terrain,
        Bucket::Scatter,
        Bucket::Particles,
        Bucket::Audio,
        Bucket::Animation,
        Bucket::Models,
        Bucket::Ui,
        Bucket::Render,
        Bucket::Assets,
        Bucket::GpuWait,
        Bucket::Present,
        Bucket::Other,
    ];

    /// The name a script passes to `perf.ms(...)`, and the label a readout shows.
    ///
    /// What a game author calls them, not what the crates are called, and
    /// camelCase where a name is two words.
    pub fn name(self) -> &'static str {
        match self {
            Bucket::Scripts => "scripts",
            Bucket::Mirror => "mirror",
            Bucket::Physics => "physics",
            Bucket::Terrain => "terrain",
            Bucket::Scatter => "scatter",
            Bucket::Particles => "particles",
            Bucket::Audio => "audio",
            Bucket::Animation => "animation",
            Bucket::Models => "models",
            Bucket::Ui => "ui",
            Bucket::Render => "render",
            Bucket::Assets => "assets",
            Bucket::GpuWait => "gpuWait",
            Bucket::Present => "present",
            Bucket::Other => "untimed",
        }
    }

    /// Resolve a name from a script. `None` for anything unrecognised — the
    /// caller turns that into an error naming the whole set, rather than
    /// answering zero for a typo.
    pub fn from_name(name: &str) -> Option<Bucket> {
        if name == "other" {
            return Some(Bucket::Other);
        }
        Bucket::ALL.into_iter().find(|b| b.name() == name)
    }
}

/// A frame that takes this many milliseconds or more, by the time between
/// frames or by its buckets, is a hitch, and is kept for
/// [`FrameProfile::take_hitches`]. The default: a game aiming at 30 or 60 fps
/// lowers it with [`FrameProfile::set_hitch_threshold`] (`perf.hitchThreshold`).
pub const HITCH_MS: f32 = 50.0;
/// How many frames [`FrameProfile::recent_frames`] (`perf.frames(n)`) can look
/// back: five seconds at 60 fps.
pub const FRAME_RING: usize = 300;
/// How many sampled lines a frame record keeps, costliest first.
pub const FRAME_SAMPLES: usize = 10;
/// How many assemblies and bodies a frame record keeps, costliest first.
pub const FRAME_BODIES: usize = 16;
/// The most distinct lines the sampler holds in one frame or between two reads
/// of [`FrameProfile::take_samples`]; past that, new lines are folded into one
/// row named `(other)`, so a script with thousands of hot lines cannot grow
/// the profile without bound.
pub const MAX_SAMPLE_LINES: usize = 512;

/// Engine calls one script made in a frame: `n` calls of `call` that took
/// `ms` between them. A part of that script's own time, not beside it.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineCall {
    /// The script that made the calls, by file name.
    pub script: String,
    /// The binding: `raycast`, `spherecast`, …
    pub call: &'static str,
    pub n: u64,
    pub ms: f32,
}

/// One line the script sampler landed on, and the time that went to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The script, by file name.
    pub script: String,
    /// The function the line is in, or `?` for a chunk's main body or an
    /// anonymous function.
    pub func: String,
    pub line: u32,
    /// Samples that landed here.
    pub n: u32,
    /// The wall time those samples stood for: each sample carries the time
    /// since the one before it, so an engine call that ran long is charged to
    /// the line that made it.
    pub ms: f32,
}

/// One frame, exactly, as `perf.frames(n)` returns it: no smoothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameRecord {
    /// Which frame, counted since collection started.
    pub frame: u64,
    /// The buckets added up, `untimed` included: the time from the end of the
    /// frame before to the end of this one.
    pub ms: f32,
    /// Every bucket that spent anything, in [`Bucket::ALL`] order.
    pub buckets: Vec<(Bucket, f32)>,
    /// The named parts, in the order they were first reported.
    pub parts: Vec<(&'static str, f32)>,
    /// Each script's hook time this frame, costliest first.
    pub scripts: Vec<(String, f32)>,
    /// The engine calls each script made, costliest first.
    pub calls: Vec<EngineCall>,
    /// The hottest sampled lines, costliest first, at most [`FRAME_SAMPLES`];
    /// empty unless the sampler is on.
    pub samples: Vec<Sample>,
    /// The costliest physics assemblies and bodies, by node name, at most
    /// [`FRAME_BODIES`].
    pub bodies: Vec<(String, f32)>,
    pub counts: Counts,
    /// What held the frame's rate, when it was not the engine's own work:
    /// `display` (vsync), `compositor` (a window system pacing the window
    /// slower than the display, as a hidden or locked session does),
    /// `frameCap` (`app.setFrameCap`) or `gpu`. See [`pacing_cause`].
    pub paced: Option<&'static str>,
}

impl FrameRecord {
    /// One bucket's milliseconds this frame, zero when it spent nothing.
    pub fn bucket(&self, b: Bucket) -> f32 {
        self.buckets.iter().find(|(x, _)| *x == b).map(|(_, ms)| *ms).unwrap_or(0.0)
    }

    /// One part's milliseconds this frame, zero when it did not run.
    pub fn part(&self, name: &str) -> f32 {
        self.parts.iter().find(|(p, _)| *p == name).map(|(_, ms)| *ms).unwrap_or(0.0)
    }
}

/// How the frame is shown, as the driver set it up: what
/// [`FrameProfile::pacing`] reports beside its verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PacingSetup {
    /// The swapchain's present mode: `fifo`, `fifoRelaxed`, `mailbox`,
    /// `immediate`, or `none` when nothing presents (a headless run).
    pub present_mode: &'static str,
    /// Whether that mode waits for the display.
    pub vsync: bool,
    /// The display's refresh rate, 0 when unknown.
    pub refresh_hz: f32,
    /// `app.setFrameCap`, 0 when off.
    pub frame_cap: f32,
}

/// What the frame's pacing looked like, last frame and recently.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pacing {
    pub setup: PacingSetup,
    /// The last frame, end to end.
    pub frame_ms: f32,
    /// …of which waiting for a swapchain image and presenting it.
    pub wait_ms: f32,
    /// The GPU's own time for the latest frame it timed, if any.
    pub gpu_ms: Option<f32>,
    /// What held the last frame, if anything did ([`pacing_cause`]).
    pub limited_by: Option<&'static str>,
    /// Of the last [`WINDOW`] frames, how many something other than the
    /// engine's own work held.
    pub paced_frames: usize,
    /// …out of this many.
    pub frames: usize,
}

/// What held a frame to its length, when the engine's own work did not.
///
/// A frame mostly spent waiting for a swapchain image is not slow because of
/// anything the game did. Waiting about one refresh under a vsync mode is the
/// `display`; waiting longer than one refresh with a GPU that finished early
/// (or that cannot say) is a `compositor` handing out fewer frames than the
/// display shows, which is what a hidden window or a locked session looks
/// like; a GPU whose own time fills the frame is `gpu`; a frame cap's sleep is
/// `frameCap`. A frame whose waits are under 40% of it was the engine's work,
/// and gets `None`.
pub fn pacing_cause(
    frame_ms: f32,
    wait_ms: f32,
    cap_wait_ms: f32,
    gpu_ms: Option<f32>,
    refresh_ms: f32,
    vsync: bool,
) -> Option<&'static str> {
    if frame_ms <= 0.0 {
        return None;
    }
    if cap_wait_ms >= 0.4 * frame_ms {
        return Some("frameCap");
    }
    if wait_ms < 0.4 * frame_ms {
        return None;
    }
    if gpu_ms.is_some_and(|g| g >= 0.7 * frame_ms) {
        return Some("gpu");
    }
    if !vsync {
        // Without vsync a wait for an image is the GPU still drawing, unless
        // the GPU says it finished early.
        return Some(if gpu_ms.is_some() { "compositor" } else { "gpu" });
    }
    if refresh_ms > 0.0 && frame_ms <= 1.25 * refresh_ms {
        return Some("display");
    }
    Some("compositor")
}
/// The most hitches kept between two reads; past that the oldest go.
pub const MAX_HITCHES: usize = 64;

/// One slow frame, as the buckets saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct Hitch {
    /// Which frame, counted since collection started.
    pub frame: u64,
    /// The buckets added up.
    pub ms: f32,
    /// Every bucket that spent anything, most expensive first.
    pub buckets: Vec<(Bucket, f32)>,
    /// Named pieces of work inside the buckets ([`FrameProfile::record_part`]),
    /// most expensive first: what in `terrain` the time went to.
    pub parts: Vec<(&'static str, f32)>,
    /// The slowest assets prepared on this frame ([`FrameProfile::record_asset`]),
    /// slowest first and at most [`HITCH_ASSETS`]: the file, which part of
    /// the work it was (`assetDecode`, `assetUpload`, `shaderCompile`,
    /// `pipelineCompile`), and its milliseconds.
    pub assets: Vec<(String, &'static str, f32)>,
    /// Each script's hook time, costliest first.
    pub scripts: Vec<(String, f32)>,
    /// The engine calls the scripts made, costliest first.
    pub calls: Vec<EngineCall>,
    /// The hottest sampled lines, when the sampler is on.
    pub samples: Vec<Sample>,
    /// The costliest physics assemblies and bodies, by node name.
    pub bodies: Vec<(String, f32)>,
}

/// How many assets a hitch names.
pub const HITCH_ASSETS: usize = 5;

/// How many frames the "worst recently" window covers.
///
/// One second at 60 fps. Long enough that a hitch does not scroll off before you
/// have read it, short enough that fixing one is visibly reflected.
pub const WINDOW: usize = 60;

/// The smoothing factor for the rolling mean, per frame.
///
/// The same 0.9/0.1 the FPS counter has always used, so the two move at the same
/// rate and a reader is not comparing a fast number with a slow one.
const SMOOTH: f32 = 0.9;

/// One bucket's history: a smoothed mean and a ring of recent samples.
#[derive(Clone, Debug, Default)]
struct Series {
    mean: f32,
    ring: Vec<f32>,
    next: usize,
}

impl Series {
    fn push(&mut self, ms: f32) {
        self.mean = if self.ring.is_empty() { ms } else { self.mean * SMOOTH + ms * (1.0 - SMOOTH) };
        if self.ring.len() < WINDOW {
            self.ring.push(ms);
        } else {
            self.ring[self.next] = ms;
            self.next = (self.next + 1) % WINDOW;
        }
    }

    fn worst(&self) -> f32 {
        self.ring.iter().copied().fold(0.0, f32::max)
    }
}

/// What one bucket cost: the rolling mean, and the worst frame of the last
/// [`WINDOW`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    /// Milliseconds, smoothed.
    pub ms: f32,
    /// Milliseconds, the worst single frame in the window. **This is the number
    /// worth looking at** — a hitch is invisible in the mean.
    pub worst_ms: f32,
}

/// The counts a frame produced.
///
/// Separate from the times because three of the four misdiagnosed tickets were
/// answerable from a count alone, and a count is free to keep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Scene nodes walked by the draw gather.
    pub nodes: usize,
    /// …rejected as off screen before any work was done for them.
    pub culled: usize,
    /// Raster instances submitted, terrain and scatter included.
    pub instances: usize,
    /// Draw calls issued.
    pub draws: usize,
    /// Terrain chunks resident (meshed and in memory).
    pub chunks: usize,
    /// Scatter props submitted this frame. A field can ask for 117,000 of
    /// these, and this is how you see the number.
    pub props: usize,
    /// Live particles across every effect.
    pub particles: usize,
    /// Live one-shot particle effects — `spawnEffect` instances that have not
    /// finished. `particles` above is what they cost; this is how many asked.
    pub effects: usize,
    /// …and how many a frame refused because the ceiling was already reached.
    /// Nonzero means the look is being cut, so it cannot be a number nobody can
    /// see.
    pub effects_dropped: usize,
    /// Placeable lights the shader was given, of the sixteen slots it has.
    pub lights: usize,
    /// …and how many were ranked out because more than sixteen qualified. A cap
    /// that says so is a good cap.
    pub lights_dropped: usize,
    /// Audio voices mixing.
    pub voices: usize,
    /// Flat surfaces filled into the 2D lighting G-buffer — the second
    /// rasterization a lit 2D scene pays for.
    ///
    /// `0` in a scene with no 2D light placed, and `0` for every surface no live
    /// light's layer mask can reach, so this reads as *what the 2D lighting is
    /// costing you right now* rather than as how much flat matter you own. A
    /// bullet hell was paying ~500 of these a frame against lights that could
    /// reach none of them, and had no number to see it with.
    pub flat2d: usize,
    /// Shadow proxies the march was given: the collider boxes, spheres and
    /// capsules nearest the camera, of the 32 slots it has. Each one is tested
    /// along every shadow ray that passes near it, so beside a building made of
    /// many boxes this is where lighting time goes.
    pub shadow_proxies: usize,
    /// …and how many casters were left out because more than 32 qualified.
    pub shadow_proxies_dropped: usize,
    /// Baked volumes the shadow march reads: terrain and mesh occluder bakes.
    pub shadow_volumes: usize,
    /// Draw calls that carried a single instance, and why each did not share
    /// a draw with another. `draws` far above the number of distinct meshes
    /// is the symptom; this is the cause.
    pub unbatched: Unbatched,
}

/// Single-instance draws by the reason they did not join a batch.
///
/// Meshes batch by mesh, then by the images bound with it, then by shader and
/// that shader's binding. Colour, roughness, emissive, tint, vertex paint and
/// every other per-material number ride in the instance and never split a
/// batch; two materials that name the same images and the same shader with the
/// same parameters share one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unbatched {
    /// Nothing else this frame draws the same mesh, so there was no batch to
    /// join. Normal for one-off scenery; terrain chunks are counted apart.
    pub unique_mesh: usize,
    /// Other draws of the same mesh bind different images: a texture or
    /// surface map that differs.
    pub texture: usize,
    /// Other draws of the same mesh use another `.flsl` shader, or the
    /// built-in shading where this one names a shader (or the reverse).
    pub shader: usize,
    /// Other draws of the same mesh use the same shader with different
    /// shader parameters or shader textures.
    pub shader_params: usize,
    /// Other draws of the same mesh are skinned where this one is not, or the
    /// reverse.
    pub skinned: usize,
}

impl Unbatched {
    /// All of them.
    pub fn total(&self) -> usize {
        self.unique_mesh + self.texture + self.shader + self.shader_params + self.skinned
    }

    /// Each reason by the name `perf.counts().unbatchedBy` gives it.
    pub fn reasons(&self) -> [(&'static str, usize); 5] {
        [
            ("uniqueMesh", self.unique_mesh),
            ("texture", self.texture),
            ("shader", self.shader),
            ("shaderParams", self.shader_params),
            ("skinned", self.skinned),
        ]
    }
}

/// A frame's cost, per bucket and per script.
///
/// Written by the driver through [`FrameProfile::record`] / [`Self::begin`], read
/// by the editor readout and by Lua. Silent — and free — until [`Self::enable`].
#[derive(Clone, Debug, Default)]
pub struct FrameProfile {
    on: bool,
    series: HashMap<Bucket, Series>,
    /// This frame's accumulation, before `end_frame` folds it into `series`. A
    /// bucket may be timed in several pieces (physics runs per tick, scripts run
    /// three passes) and they all belong to one frame's figure.
    frame: HashMap<Bucket, f32>,
    /// Per script kind — the file name, which is what a game author has words
    /// for. Same two-phase accumulation as the buckets.
    script_frame: HashMap<String, f32>,
    scripts: HashMap<String, Series>,
    counts: Counts,
    /// Frames folded since collection was turned on, so a reader can tell
    /// "nothing has happened yet" from "it costs nothing".
    frames: u64,
    /// The script mirror's work this frame, and last frame's once folded.
    mirror_frame: MirrorWork,
    mirror_last: MirrorWork,
    /// Why the mirror last rebuilt from scratch. Kept whether or not the
    /// profiler is on: it is written only when a rebuild happens, and the
    /// question is usually asked after the fact.
    mirror_cause: Option<String>,
    /// The fixed tick costs more than the time it simulates. Written by the
    /// driver whether or not collection is on: it is a state, not a measurement.
    overloaded: bool,
    /// Last frame's animators and how many were culled.
    anim_counts: (usize, usize),
    /// Decals in the world and the triangles they were laid as.
    decal_counts: (usize, usize),
    /// Rays scripts cast this frame and the milliseconds they took, and last
    /// frame's once folded. Inside the `scripts` bucket, not beside it.
    rays_frame: (u64, f32),
    rays_last: (u64, f32),
    /// Sweeps scripts cast this frame (`spherecast`, `capsulecast`,
    /// `boxcast`) and their milliseconds, the same way as the rays.
    sweeps_frame: (u64, f32),
    sweeps_last: (u64, f32),
    /// Frames whose buckets added up to [`HITCH_MS`] or more, kept until a
    /// script takes them: the one reading a probe polling every few seconds
    /// can still see, after the spike has rolled out of the window.
    /// When the last frame ended, for the time between frames: what the
    /// buckets add up to is set against it, and the rest is [`Bucket::Other`].
    last_end: Option<crate::time::Instant>,
    hitches: std::collections::VecDeque<Hitch>,
    /// This frame's named parts, folded into a hitch when it is one.
    parts_frame: Vec<(&'static str, f32)>,
    /// The last finished frame's parts, every frame, for `perf.parts()`.
    parts_last: Vec<(&'static str, f32)>,
    /// This frame's slowest assets, kept to [`HITCH_ASSETS`] as they come.
    assets_frame: Vec<(String, &'static str, f32)>,
    /// The GPU's own per-pass times for the last frame whose timestamps have
    /// landed, and their total. `None` when nothing measured them: timing off,
    /// or a device without timestamp queries.
    gpu: Option<(Vec<(String, f32)>, f32)>,
    /// Frames in a row whose mirror rebuilt from scratch at least once.
    /// Counted whether or not collection is on: it feeds a warning, not a
    /// readout.
    rebuild_streak: u32,
    /// What a hitch is, when a game has said ([`Self::set_hitch_threshold`]);
    /// [`HITCH_MS`] otherwise.
    hitch_ms: Option<f32>,
    /// The last [`FRAME_RING`] frames, exactly, oldest first.
    ring: std::collections::VecDeque<FrameRecord>,
    /// Engine calls each script made this frame, and last frame's.
    calls_frame: Vec<EngineCall>,
    calls_last: Vec<EngineCall>,
    /// The script sampler: on while `Some`, with the interval in ms.
    sampling: Option<f32>,
    /// This frame's sampled lines, and every one since the last
    /// [`Self::take_samples`].
    samples_frame: Vec<Sample>,
    samples_total: Vec<Sample>,
    /// Physics owners' cost this frame by node name, and last frame's.
    bodies_frame: Vec<(String, f32)>,
    bodies_last: Vec<(String, f32)>,
    /// When the driver began this frame ([`Self::begin_frame`]): the time
    /// before it, back to the last frame's end, was spent outside the frame.
    began: Option<crate::time::Instant>,
    /// How frames are shown, as the driver last said, and the last verdict.
    pacing_setup: PacingSetup,
    pacing_last: Pacing,
}

/// What the script mirror did over one frame.
///
/// A full rebuild is O(scene); a refresh is O(entities that changed). A game
/// that sees `rebuilds` climbing every frame has something structural changing
/// every frame, and [`FrameProfile::mirror_cause`] names it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MirrorWork {
    /// Times the whole mirror was rebuilt.
    pub rebuilds: u32,
    /// Entities re-read individually because one of their components changed.
    pub refreshed: u32,
}

impl FrameProfile {
    /// Turn collection on or off. Off is the default and costs nothing.
    ///
    /// Turning it off clears the history rather than freezing it: a stale mean
    /// from before a fix looks exactly like a fix that did not work.
    pub fn enable(&mut self, on: bool) {
        if self.on == on {
            return;
        }
        self.on = on;
        self.series.clear();
        self.scripts.clear();
        self.frame.clear();
        self.script_frame.clear();
        self.counts = Counts::default();
        self.frames = 0;
        self.mirror_frame = MirrorWork::default();
        self.mirror_last = MirrorWork::default();
        self.ring.clear();
        self.calls_frame.clear();
        self.calls_last.clear();
        self.samples_frame.clear();
        self.samples_total.clear();
        self.bodies_frame.clear();
        self.bodies_last.clear();
        self.parts_frame.clear();
        self.parts_last.clear();
        self.pacing_last = Pacing::default();
        if !on {
            self.sampling = None;
        }
    }

    /// The frame time at or over which a frame is kept as a hitch: [`HITCH_MS`]
    /// unless a game has set its own.
    pub fn hitch_threshold(&self) -> f32 {
        self.hitch_ms.unwrap_or(HITCH_MS)
    }

    /// Keep every frame of `ms` or more as a hitch: 33 for a game aiming at
    /// 30 fps, 20 for one aiming at 60. Clamped to at least 1 ms, so a
    /// threshold of zero cannot make every frame a hitch and push the real
    /// ones out of the log.
    pub fn set_hitch_threshold(&mut self, ms: f32) {
        self.hitch_ms = Some(if ms.is_finite() { ms.max(1.0) } else { HITCH_MS });
    }

    /// Mark the start of a frame's own work. The time from the end of the
    /// frame before to here was spent outside the engine's frame: the window
    /// system's events, and the OS or compositor not running or not asking
    /// for the frame. It is named `outsideFrame` inside `untimed`.
    pub fn begin_frame(&mut self) {
        if self.on {
            self.began = Some(crate::time::Instant::now());
        }
    }

    /// `ms` of this frame went to a wait no bucket owns, named `what`:
    /// counted into `untimed` and named there as a part, so the frame's
    /// untimed time says what it was where the engine knows.
    pub fn record_wait(&mut self, what: &'static str, ms: f32) {
        if !self.on {
            return;
        }
        *self.frame.entry(Bucket::Other).or_default() += ms;
        self.record_part(what, ms);
    }

    /// Every bucket's total for the frame in progress added up. Read it
    /// either side of a piece of work to time that work net of the buckets
    /// recorded inside it, as [`Self::record_wait`] needs.
    pub fn frame_measured(&self) -> f32 {
        self.frame.values().sum()
    }

    /// `n` calls of the binding `call` that `script` made, taking `ms` between
    /// them: what of a script's own time went to the engine on its behalf.
    /// Nothing while off.
    pub fn record_call(&mut self, script: &str, call: &'static str, n: u64, ms: f32) {
        if !self.on {
            return;
        }
        match self.calls_frame.iter_mut().find(|c| c.call == call && c.script == script) {
            Some(c) => {
                c.n += n;
                c.ms += ms;
            }
            None => self.calls_frame.push(EngineCall { script: script.to_string(), call, n, ms }),
        }
    }

    /// The engine calls each script made last frame, costliest first.
    pub fn last_calls(&self) -> &[EngineCall] {
        &self.calls_last
    }

    /// Start or stop the script sampler, sampling every `interval_ms` of a
    /// script's run time. Only meaningful while collection is on.
    pub fn set_sampling(&mut self, interval_ms: Option<f32>) {
        self.sampling = if self.on { interval_ms.map(|ms| ms.clamp(0.01, 50.0)) } else { None };
        if self.sampling.is_none() {
            self.samples_frame.clear();
        }
    }

    /// The sampler's interval in milliseconds while it is on.
    pub fn sampling(&self) -> Option<f32> {
        self.sampling
    }

    /// One sample: the script was at `line` of `func` in `script`, standing
    /// for `ms` of wall time since the sample before it.
    pub fn record_sample(&mut self, script: &str, func: &str, line: u32, ms: f32) {
        if !self.on || self.sampling.is_none() {
            return;
        }
        add_sample(&mut self.samples_frame, script, func, line, 1, ms);
    }

    /// Every sampled line since the last call, costliest first.
    pub fn take_samples(&mut self) -> Vec<Sample> {
        let mut v = std::mem::take(&mut self.samples_total);
        v.sort_by(|a, b| b.ms.total_cmp(&a.ms));
        v
    }

    /// `ms` of physics this frame went to the body or assembly whose node is
    /// called `name`. Nothing while off.
    pub fn record_body(&mut self, name: &str, ms: f32) {
        if !self.on {
            return;
        }
        match self.bodies_frame.iter_mut().find(|(n, _)| n == name) {
            Some((_, v)) => *v += ms,
            None => self.bodies_frame.push((name.to_string(), ms)),
        }
    }

    /// Last frame's physics owners by node name, costliest first.
    pub fn last_bodies(&self) -> &[(String, f32)] {
        &self.bodies_last
    }

    /// The last `n` frames, oldest first, at most [`FRAME_RING`].
    pub fn recent_frames(&self, n: usize) -> impl Iterator<Item = &FrameRecord> {
        let skip = self.ring.len().saturating_sub(n);
        self.ring.iter().skip(skip)
    }

    /// Say how frames are shown: the present mode, whether it waits for the
    /// display, the display's rate and the frame cap. Read by
    /// [`Self::pacing`] and by each frame's verdict.
    pub fn set_pacing_setup(&mut self, setup: PacingSetup) {
        self.pacing_setup = setup;
    }

    /// The last frame's pacing and the recent share of frames something other
    /// than the engine held.
    pub fn pacing(&self) -> Pacing {
        Pacing { setup: self.pacing_setup, ..self.pacing_last }
    }

    /// Is anything being collected?
    pub fn enabled(&self) -> bool {
        self.on
    }

    /// How many frames have been folded in since collection started. Zero means
    /// "ask again next frame", which is not the same as "it is free".
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Add `ms` to a bucket's total for the frame in progress. A no-op while
    /// disabled, which is what makes the instrumentation free.
    pub fn record(&mut self, bucket: Bucket, ms: f32) {
        if !self.on {
            return;
        }
        *self.frame.entry(bucket).or_default() += ms;
    }

    /// The named parts of the last finished frame and what each cost, in ms:
    /// work inside a bucket a game can watch frame by frame, where a hitch
    /// only shows it on a slow frame.
    pub fn last_parts(&self) -> &[(&'static str, f32)] {
        &self.parts_last
    }

    /// Name a piece of work inside a bucket: `ms` of what the bucket already
    /// counts went to `part`. Not added to any bucket. A slow frame keeps its
    /// parts, so a hitch says which work in the bucket it was, where the bucket
    /// alone says only "terrain".
    pub fn record_part(&mut self, part: &'static str, ms: f32) {
        if !self.on {
            return;
        }
        match self.parts_frame.iter_mut().find(|(p, _)| *p == part) {
            Some((_, v)) => *v += ms,
            None => self.parts_frame.push((part, ms)),
        }
    }

    /// One asset prepared this frame: `ms` spent on `what` (a part name, as
    /// [`Self::record_part`] takes) for the file at `path`. Counted into that
    /// part, and the slowest few of a frame are kept, so a hitch names the
    /// file it was waiting on rather than only the bucket.
    pub fn record_asset(&mut self, path: &str, what: &'static str, ms: f32) {
        if !self.on {
            return;
        }
        self.record_part(what, ms);
        let full = self.assets_frame.len() >= HITCH_ASSETS;
        if full && self.assets_frame.last().is_some_and(|l| l.2 >= ms) {
            return;
        }
        if full {
            self.assets_frame.pop();
        }
        let at = self.assets_frame.iter().position(|a| a.2 < ms).unwrap_or(self.assets_frame.len());
        self.assets_frame.insert(at, (path.to_string(), what, ms));
    }

    /// Add `ms` to one script kind's total.
    ///
    /// Not to `Bucket::Scripts`. Adding to both would make the per-script
    /// figures sum exactly to the bucket, and make the bucket hook time only,
    /// with everything the engine does to reach a hook invisible: a real game
    /// would account 5.3 of 12.9 ms. `Bucket::Scripts` is the wall clock of
    /// the whole pass, recorded once per pass by the caller that runs it, and
    /// these figures are a breakdown of the part of it inside a hook.
    pub fn record_script(&mut self, kind: &str, ms: f32) {
        if !self.on {
            return;
        }
        *self.script_frame.entry(kind.to_owned()).or_default() += ms;
    }

    /// Note one pass of the script mirror: a full rebuild (with why), or a
    /// refresh of `refreshed` entities.
    /// Frames in a row, up to the last one folded, that rebuilt the script
    /// mirror from scratch. A game doing something structural every frame.
    pub fn mirror_rebuild_streak(&self) -> u32 {
        self.rebuild_streak
    }

    pub fn record_mirror(&mut self, rebuilt: Option<String>, refreshed: u32) {
        if let Some(cause) = rebuilt {
            self.mirror_frame.rebuilds += 1;
            self.mirror_cause = Some(cause);
        }
        self.mirror_frame.refreshed += refreshed;
    }

    /// Last complete frame's mirror work. Zero while collection is off.
    pub fn mirror_work(&self) -> MirrorWork {
        self.mirror_last
    }

    /// Why the mirror last rebuilt from scratch, if it ever has.
    pub fn mirror_cause(&self) -> Option<&str> {
        self.mirror_cause.as_deref()
    }

    /// Each script kind's total for the frame in progress, before `end_frame`
    /// folds it. For a caller keeping its own distribution across a run.
    pub fn frame_scripts(&self) -> impl Iterator<Item = (&str, f32)> {
        self.script_frame.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Last frame's animators, and how many culling skipped.
    pub fn set_anim_counts(&mut self, animators: usize, culled: usize) {
        self.anim_counts = (animators, culled);
    }

    /// Count `n` script rays that took `ms` between them. Nothing while off.
    pub fn record_rays(&mut self, n: u64, ms: f32) {
        if self.on {
            self.rays_frame.0 += n;
            self.rays_frame.1 += ms;
        }
    }

    /// Count `n` script sweeps that took `ms` between them. Nothing while off.
    pub fn record_sweeps(&mut self, n: u64, ms: f32) {
        if self.on {
            self.sweeps_frame.0 += n;
            self.sweeps_frame.1 += ms;
        }
    }

    /// Publish the GPU's per-pass times, `(label, ms)` in pass order, and the
    /// frame's total.
    pub fn set_gpu(&mut self, passes: Vec<(String, f32)>, total_ms: f32) {
        self.gpu = Some((passes, total_ms));
    }

    /// The last measured GPU frame, if any was: see [`Self::set_gpu`].
    pub fn gpu(&self) -> Option<&(Vec<(String, f32)>, f32)> {
        self.gpu.as_ref()
    }

    /// `(rays, ms)` scripts cast last frame.
    pub fn rays(&self) -> (u64, f32) {
        self.rays_last
    }

    /// `(sweeps, ms)` scripts cast last frame.
    pub fn sweeps(&self) -> (u64, f32) {
        self.sweeps_last
    }

    /// `(animators, culled)` as of the last animation pass.
    pub fn anim_counts(&self) -> (usize, usize) {
        self.anim_counts
    }

    /// Decals in the world and their triangles, whenever they change.
    pub fn set_decal_counts(&mut self, decals: usize, triangles: usize) {
        self.decal_counts = (decals, triangles);
    }

    /// `(decals, triangles)`.
    pub fn decal_counts(&self) -> (usize, usize) {
        self.decal_counts
    }

    /// Say whether the game has fallen behind real time.
    pub fn set_overloaded(&mut self, on: bool) {
        self.overloaded = on;
    }

    /// Has the game fallen behind real time? See `floptle_core::TickLoad`.
    pub fn overloaded(&self) -> bool {
        self.overloaded
    }

    /// The costliest buckets, largest first, with the costliest script named
    /// beside `scripts`: `"physics 17.2 ms · scripts 9.0 ms (enemy 6.7) · mirror
    /// 5.4 ms"`. For a message a person reads, so it only lists what cost
    /// something. Empty while collection is off.
    pub fn top_costs(&self, n: usize) -> String {
        if !self.on {
            return String::new();
        }
        let mut rows: Vec<(Bucket, f32)> = Bucket::ALL
            .iter()
            .filter_map(|b| self.bucket(*b).map(|c| (*b, c.ms)))
            .filter(|(_, ms)| *ms >= 0.05)
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
        rows.truncate(n);
        let top_script = self.scripts().into_iter().next();
        rows.iter()
            .map(|(b, ms)| match (&top_script, b) {
                (Some((name, c)), Bucket::Scripts) if c.ms >= 0.05 => {
                    format!("{} {ms:.1} ms ({name} {:.1})", b.name(), c.ms)
                }
                _ => format!("{} {ms:.1} ms", b.name()),
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// Publish the live particle and one-shot effect counts from the
    /// simulation step, which runs whether or not the frame is drawn. The draw
    /// gather publishes them again with everything else; without this a frame
    /// nobody drew — every frame of `floptle run`, most of a `shot --after` —
    /// read zero particles while thousands were live.
    pub fn set_particle_counts(&mut self, particles: usize, effects: usize) {
        if self.on {
            self.counts.particles = particles;
            self.counts.effects = effects;
        }
    }

    /// Publish this frame's counts.
    pub fn set_counts(&mut self, counts: Counts) {
        if self.on {
            self.counts = counts;
        }
    }

    /// Fold the frame in progress into the history. Called once per rendered
    /// frame, after everything has reported.
    pub fn end_frame(&mut self) {
        self.rebuild_streak = if self.mirror_frame.rebuilds > 0 { self.rebuild_streak.saturating_add(1) } else { 0 };
        let now = crate::time::Instant::now();
        let last_end = self.last_end.replace(now);
        let between = last_end.map(|t| (now - t).as_secs_f32() * 1000.0);
        let began = self.began.take();
        if !self.on {
            self.mirror_frame = MirrorWork::default();
            self.rays_frame = (0, 0.0);
            self.sweeps_frame = (0, 0.0);
            return;
        }
        // The time before this frame's own work began, back to the end of the
        // one before: outside the frame, named as such inside `untimed`.
        if let (Some(t0), Some(b)) = (last_end, began)
            && b > t0
        {
            self.record_wait("outsideFrame", (b - t0).as_secs_f32() * 1000.0);
        }
        // The frame's time no bucket measured. Recorded by whoever ended the
        // frame calling here, so it is the real gap between two frames.
        if let Some(between) = between {
            let rest = between - self.frame_measured();
            if rest > 0.0 {
                *self.frame.entry(Bucket::Other).or_insert(0.0) += rest;
            }
        }
        self.mirror_last = std::mem::take(&mut self.mirror_frame);
        self.rays_last = std::mem::take(&mut self.rays_frame);
        self.sweeps_last = std::mem::take(&mut self.sweeps_frame);
        // Every bucket is pushed, including the ones that reported nothing —
        // otherwise a subsystem that went idle keeps its old mean forever and
        // reads as still costing what it used to.
        let mut spent: Vec<(Bucket, f32)> = Vec::new();
        for b in Bucket::ALL {
            let ms = self.frame.remove(&b).unwrap_or(0.0);
            self.series.entry(b).or_default().push(ms);
            if ms > 0.0 {
                spent.push((b, ms));
            }
        }
        let total: f32 = spent.iter().map(|(_, ms)| ms).sum();
        let parts = std::mem::take(&mut self.parts_frame);
        let assets = std::mem::take(&mut self.assets_frame);
        self.parts_last.clone_from(&parts);
        let mut calls = std::mem::take(&mut self.calls_frame);
        calls.sort_by(|a, b| b.ms.total_cmp(&a.ms));
        self.calls_last.clone_from(&calls);
        let mut samples = std::mem::take(&mut self.samples_frame);
        for x in &samples {
            add_sample(&mut self.samples_total, &x.script, &x.func, x.line, x.n, x.ms);
        }
        samples.sort_by(|a, b| b.ms.total_cmp(&a.ms));
        samples.truncate(FRAME_SAMPLES);
        let mut bodies = std::mem::take(&mut self.bodies_frame);
        bodies.sort_by(|a, b| b.1.total_cmp(&a.1));
        bodies.truncate(FRAME_BODIES);
        self.bodies_last.clone_from(&bodies);
        let mut scripts: Vec<(String, f32)> = self.script_frame.iter().map(|(k, v)| (k.clone(), *v)).collect();
        scripts.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        // What held the frame, judged from its own waits.
        let wait: f32 = spent.iter().filter(|(b, _)| matches!(b, Bucket::GpuWait | Bucket::Present)).map(|(_, ms)| ms).sum();
        let cap: f32 = parts.iter().filter(|(p, _)| *p == "frameCap").map(|(_, ms)| *ms).sum();
        let gpu_ms = self.gpu.as_ref().map(|g| g.1);
        let refresh_ms = if self.pacing_setup.refresh_hz > 0.0 { 1000.0 / self.pacing_setup.refresh_hz } else { 0.0 };
        let paced = pacing_cause(total, wait, cap, gpu_ms, refresh_ms, self.pacing_setup.vsync);
        let record = FrameRecord {
            frame: self.frames,
            ms: total,
            buckets: spent,
            parts,
            scripts,
            calls,
            samples,
            bodies,
            counts: self.counts,
            paced,
        };
        if total >= self.hitch_threshold() {
            let mut buckets = record.buckets.clone();
            buckets.sort_by(|a, b| b.1.total_cmp(&a.1));
            let mut parts = record.parts.clone();
            parts.sort_by(|a, b| b.1.total_cmp(&a.1));
            if self.hitches.len() >= MAX_HITCHES {
                self.hitches.pop_front();
            }
            self.hitches.push_back(Hitch {
                frame: self.frames,
                ms: total,
                buckets,
                parts,
                assets,
                scripts: record.scripts.clone(),
                calls: record.calls.clone(),
                samples: record.samples.clone(),
                bodies: record.bodies.clone(),
            });
        }
        if self.ring.len() >= FRAME_RING {
            self.ring.pop_front();
        }
        self.ring.push_back(record);
        let recent = self.ring.iter().rev().take(WINDOW);
        let (mut held, mut n) = (0, 0);
        for r in recent {
            n += 1;
            if matches!(r.paced, Some("display" | "compositor" | "frameCap")) {
                held += 1;
            }
        }
        self.pacing_last = Pacing {
            setup: self.pacing_setup,
            frame_ms: total,
            wait_ms: wait,
            gpu_ms,
            limited_by: paced,
            paced_frames: held,
            frames: n,
        };
        // A script that stopped running this frame gets a zero for the same
        // reason, and a script that has been destroyed stops being listed at all.
        let names: Vec<String> = self.scripts.keys().cloned().collect();
        for name in names {
            if !self.script_frame.contains_key(&name) {
                self.scripts.entry(name).or_default().push(0.0);
            }
        }
        for (name, ms) in std::mem::take(&mut self.script_frame) {
            // A script seen for the first time is back-filled with the frames it
            // was not running for. Without this its mean starts from its own first
            // sample while every bucket's started from frame one, and the two are
            // then not comparable with the rows beside it, and a script that
            // spawned a moment ago would read as the most expensive thing in the
            // scene for being the newest. It cost nothing in those frames, so
            // zero is the true value, not a filler.
            let series = self.scripts.entry(name).or_insert_with(|| {
                let mut s = Series::default();
                for _ in 0..self.frames.min(WINDOW as u64) {
                    s.push(0.0);
                }
                s
            });
            series.push(ms);
        }
        self.frames += 1;
    }

    /// Every hitch since the last call, oldest first.
    pub fn take_hitches(&mut self) -> Vec<Hitch> {
        self.hitches.drain(..).collect()
    }

    /// What a bucket cost, or `None` while collection is off.
    ///
    /// `None` and not `Cost::default()`: a game asserting `perf.ms("scripts") <
    /// 4` must not pass because nothing was measured.
    pub fn bucket(&self, b: Bucket) -> Option<Cost> {
        if !self.on {
            return None;
        }
        let s = self.series.get(&b)?;
        Some(Cost { ms: s.mean, worst_ms: s.worst() })
    }

    /// What one script kind cost, or `None` while off / if that script has never
    /// run.
    pub fn script(&self, kind: &str) -> Option<Cost> {
        if !self.on {
            return None;
        }
        let s = self.scripts.get(kind)?;
        Some(Cost { ms: s.mean, worst_ms: s.worst() })
    }

    /// Every script that has run since collection started, **most expensive
    /// first** — which is the order the question is asked in.
    pub fn scripts(&self) -> Vec<(String, Cost)> {
        if !self.on {
            return Vec::new();
        }
        let mut v: Vec<(String, Cost)> = self
            .scripts
            .iter()
            .map(|(k, s)| (k.clone(), Cost { ms: s.mean, worst_ms: s.worst() }))
            .collect();
        v.sort_by(|a, b| b.1.ms.total_cmp(&a.1.ms).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// This frame's counts. Always available in shape; zeroed while off, which is
    /// safe here in a way it is not for the times — a count of zero nodes is
    /// obviously not a measurement.
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The sum of the buckets' means, which is what the frame costs where the
    /// engine can see it. `None` while off.
    ///
    /// Not the same as the frame time: vsync, the OS and the GPU finishing are
    /// all outside every bucket. Presented as "accounted for" rather than
    /// "total" for exactly that reason — a readout claiming to add up to the
    /// frame time and not doing so is worse than one that never claimed it.
    /// A bucket's total for the frame in progress — before `end_frame` folds it
    /// into the history. Read it either side of a call to record that call net
    /// of a bucket nested inside it (`Scripts` around `Mirror`), so the buckets
    /// still sum to the frame rather than counting the inner one twice.
    pub fn frame_total(&self, bucket: Bucket) -> f32 {
        self.frame.get(&bucket).copied().unwrap_or(0.0)
    }

    pub fn accounted_ms(&self) -> Option<f32> {
        if !self.on {
            return None;
        }
        Some(Bucket::ALL.iter().filter_map(|b| self.bucket(*b)).map(|c| c.ms).sum())
    }
}

/// Fold `n` samples of `ms` at one line into a list, keeping it to
/// [`MAX_SAMPLE_LINES`] rows: past that, a new line lands in `(other)`.
fn add_sample(list: &mut Vec<Sample>, script: &str, func: &str, line: u32, n: u32, ms: f32) {
    if let Some(x) = list.iter_mut().find(|x| x.line == line && x.script == script && x.func == func) {
        x.n += n;
        x.ms += ms;
        return;
    }
    if list.len() >= MAX_SAMPLE_LINES {
        if let Some(x) = list.iter_mut().find(|x| x.script == "(other)") {
            x.n += n;
            x.ms += ms;
            return;
        }
        list.pop();
        list.push(Sample { script: "(other)".into(), func: String::new(), line: 0, n, ms });
        return;
    }
    list.push(Sample { script: script.to_string(), func: func.to_string(), line, n, ms });
}

/// A scope timer: `let _t = Span::new();` … `p.record(bucket, t.ms())`.
///
/// Deliberately not a Drop guard holding a `&mut FrameProfile` — the profile
/// lives on the editor beside the things being measured, and a guard borrowing it
/// for the length of a subsystem would fight every other borrow in the frame.
pub struct Span(crate::time::Instant);

impl Span {
    pub fn new() -> Self {
        Span(crate::time::Instant::now())
    }

    /// Milliseconds since the span started.
    pub fn ms(&self) -> f32 {
        self.0.elapsed().as_secs_f32() * 1000.0
    }
}

impl Default for Span {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// While off, times read as absent rather than zero — so a smoke test
    /// asserting a budget cannot pass because nothing was measured.
    ///
    /// The failure mode designed out here is a number that means "no data"
    /// and looks like "free".
    #[test]
    fn a_disabled_profile_answers_nothing_rather_than_zero() {
        let mut p = FrameProfile::default();
        assert!(!p.enabled());
        p.record(Bucket::Scripts, 12.0);
        p.record_script("player", 12.0);
        p.end_frame();
        assert_eq!(p.bucket(Bucket::Scripts), None, "a budget check must not pass on no data");
        assert_eq!(p.script("player"), None);
        assert_eq!(p.accounted_ms(), None);
        assert!(p.scripts().is_empty());
        assert_eq!(p.frames(), 0);
    }

    /// **A spike is kept until it is read, however long after.** The worst
    /// window is sixty frames, so a probe reading every few seconds lost the
    /// frame it was hunting; the hitch log still has it, with the bucket that
    /// spent the time on top, and a read empties it.
    /// **A slow frame is a hitch even when the buckets do not see why.** A
    /// frame of 60 ms whose buckets add up to 10 is kept, and the 50 ms nothing
    /// measured is named as `untimed` rather than left out, so a hitch always
    /// says where its time went, even when the answer is "not in the engine's
    /// buckets". A script that asks for it by its old name, `other`, still gets
    /// it.
    #[test]
    fn a_slow_frame_the_buckets_miss_is_a_hitch_named_untimed() {
        assert_eq!(Bucket::Other.name(), "untimed");
        assert_eq!(Bucket::from_name("other"), Some(Bucket::Other));
        assert_eq!(Bucket::from_name("gpuWait"), Some(Bucket::GpuWait));
        let mut p = FrameProfile::default();
        p.enable(true);
        p.end_frame();
        p.take_hitches();
        p.record(Bucket::Physics, 10.0);
        std::thread::sleep(std::time::Duration::from_millis(60));
        p.end_frame();
        let h = p.take_hitches();
        assert_eq!(h.len(), 1, "a 60 ms frame must be a hitch: {h:?}");
        assert!(h[0].ms >= 60.0, "the hitch is the whole frame, got {}", h[0].ms);
        let other = h[0].buckets.iter().find(|(b, _)| *b == Bucket::Other).map(|(_, ms)| *ms);
        assert!(other.is_some_and(|ms| ms >= 45.0), "the unmeasured time must be named: {h:?}");
        assert_eq!(h[0].buckets[0].0, Bucket::Other, "and it is the costliest part");
    }

    /// **A hitch names the work inside its bucket.** A mining laser's slow
    /// frames read `terrain` and nothing more; the parts a frame reported say
    /// whether it was the collider's rebuild or the edits themselves. A fast
    /// frame's parts do not carry over into the next slow one.
    #[test]
    fn a_hitch_names_the_work_inside_its_bucket() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.record(Bucket::Terrain, 2.0);
        p.record_part("terrainEdits", 2.0);
        p.end_frame();
        p.record(Bucket::Terrain, 90.0);
        p.record_part("terrainEdits", 5.0);
        p.record_part("collisionRebuild", 60.0);
        p.record_part("collisionRebuild", 25.0);
        p.end_frame();
        let h = p.take_hitches();
        assert_eq!(h.len(), 1, "{h:?}");
        assert_eq!(h[0].parts, vec![("collisionRebuild", 85.0), ("terrainEdits", 5.0)]);
    }

    #[test]
    fn a_hitch_is_kept_until_it_is_read() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.record(Bucket::Scripts, 30.0);
        p.record(Bucket::Terrain, 120.0);
        p.end_frame();
        for _ in 0..200 {
            p.record(Bucket::Render, 4.0);
            p.end_frame();
        }
        assert!(p.bucket(Bucket::Terrain).unwrap().worst_ms < 0.1, "the window has rolled past it");
        let h = p.take_hitches();
        assert_eq!(h.len(), 1, "{h:?}");
        assert_eq!(h[0].frame, 0);
        assert!((h[0].ms - 150.0).abs() < 1e-3);
        assert_eq!(h[0].buckets[0], (Bucket::Terrain, 120.0), "the costliest bucket first");
        assert!(p.take_hitches().is_empty(), "a read empties it");
    }

    /// The mean smooths and the worst does not — the whole reason there are two
    /// numbers. A single 40 ms hitch in a second of 2 ms frames barely moves the
    /// average and is exactly what somebody is looking for.
    #[test]
    fn a_hitch_shows_in_the_worst_and_hides_in_the_mean() {
        let mut p = FrameProfile::default();
        p.enable(true);
        for _ in 0..40 {
            p.record(Bucket::Physics, 2.0);
            p.end_frame();
        }
        p.record(Bucket::Physics, 40.0);
        p.end_frame();
        let c = p.bucket(Bucket::Physics).expect("enabled");
        assert!(c.ms < 8.0, "the mean should barely move: {}", c.ms);
        assert!((c.worst_ms - 40.0).abs() < 1e-3, "the worst must be the hitch: {}", c.worst_ms);
    }

    /// Several reports in one frame add up to that frame's figure, because
    /// physics runs per tick and scripts run three passes.
    #[test]
    fn pieces_of_one_frame_add_up() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.record(Bucket::Physics, 1.0);
        p.record(Bucket::Physics, 2.0);
        p.record(Bucket::Physics, 3.0);
        p.end_frame();
        let c = p.bucket(Bucket::Physics).expect("enabled");
        assert!((c.worst_ms - 6.0).abs() < 1e-3, "three ticks of one frame: {}", c.worst_ms);
    }

    /// Per-script times are attributed by name, and they do **not** write the
    /// `Scripts` bucket.
    ///
    /// Rows that summed to the bucket would make the bucket hook time only,
    /// with everything the engine does to reach a hook having nowhere to be.
    /// `Scripts` is the whole pass, recorded once by whoever ran it, and these
    /// rows are a breakdown of the part of it inside a hook. If
    /// `record_script` ever writes the bucket, the pass is counted twice.
    ///
    /// Watched failing by putting the bucket write back: 8.5 against the 0 here.
    #[test]
    fn scripts_are_attributed_by_name_and_do_not_write_the_bucket() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.record_script("planet_walker", 3.0);
        p.record_script("vessel_controller", 5.0);
        p.record_script("pulsate", 0.5);
        p.end_frame();
        let bucket = p.bucket(Bucket::Scripts).expect("enabled");
        assert!(
            bucket.worst_ms.abs() < 1e-3,
            "per-script rows wrote the Scripts bucket, so the pass is counted twice: {bucket:?}"
        );
        // Most expensive first: that is the order the question is asked in.
        let rows = p.scripts();
        assert_eq!(rows[0].0, "vessel_controller");
        assert_eq!(rows[1].0, "planet_walker");
        assert_eq!(rows[2].0, "pulsate");
        assert!((p.script("pulsate").unwrap().worst_ms - 0.5).abs() < 1e-3);
    }

    /// A subsystem that goes idle drops to zero instead of keeping its old mean.
    ///
    /// Otherwise fixing something looks like not fixing it: the number that made
    /// you look would sit there unchanged forever.
    #[test]
    fn an_idle_bucket_decays_instead_of_remembering() {
        let mut p = FrameProfile::default();
        p.enable(true);
        for _ in 0..30 {
            p.record(Bucket::Scatter, 20.0);
            p.record_script("forest", 20.0);
            p.end_frame();
        }
        assert!(p.bucket(Bucket::Scatter).unwrap().ms > 10.0);
        for _ in 0..200 {
            p.end_frame();
        }
        let c = p.bucket(Bucket::Scatter).unwrap();
        assert!(c.ms < 0.1, "an idle bucket still reads {} ms", c.ms);
        assert!(c.worst_ms < 0.1, "…and its window has rolled past the spike");
        assert!(
            p.script("forest").unwrap().ms < 0.1,
            "a script that stopped running still reads busy"
        );
    }

    /// Turning collection off and on again starts clean.
    #[test]
    fn re_enabling_does_not_resurrect_an_old_mean() {
        let mut p = FrameProfile::default();
        p.enable(true);
        for _ in 0..30 {
            p.record(Bucket::Render, 30.0);
            p.end_frame();
        }
        p.enable(false);
        p.enable(true);
        assert_eq!(p.bucket(Bucket::Render), None, "no samples yet, so no answer yet");
        assert_eq!(p.frames(), 0);
        p.record(Bucket::Render, 1.0);
        p.end_frame();
        assert!(p.bucket(Bucket::Render).unwrap().worst_ms < 2.0, "the old 30 ms came back");
    }

    /// Every bucket has a name, the names are unique, and they round-trip — so
    /// the editor's labels, the docs and `perf.ms("…")` cannot drift apart.
    #[test]
    fn every_bucket_name_round_trips_and_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in Bucket::ALL {
            assert!(seen.insert(b.name()), "two buckets called {}", b.name());
            assert_eq!(Bucket::from_name(b.name()), Some(b));
        }
        assert_eq!(Bucket::ALL.len(), seen.len());
        // A typo resolves to nothing, so the caller can name the whole set
        // instead of quietly measuring something else.
        assert_eq!(Bucket::from_name("scripting"), None);
        assert_eq!(Bucket::from_name("Scripts"), None);
    }

    /// A script that starts running late is comparable with one that has been
    /// running all along.
    ///
    /// The means are exponentially smoothed, so two series that began on
    /// different frames are not comparable — a script discovered on frame 30
    /// would report its full cost beside scripts averaged since frame 1, and read
    /// as the most expensive thing in the scene for being the newest. So a new
    /// script is back-filled with the frames it was idle for.
    ///
    /// `shadow` is the control: the same 0-then-4 pattern, known from frame one,
    /// which is what a correctly back-filled `latecomer` has to equal. (The
    /// original form asserted the rows summed to the `Scripts` bucket; the bucket
    /// is the whole pass now, so the property is restated against another row
    /// rather than dropped.)
    ///
    /// Watched failing with the back-fill removed: 4 against the control's 3.83.
    #[test]
    fn a_script_that_appears_late_is_comparable_with_one_that_did_not() {
        let mut p = FrameProfile::default();
        p.enable(true);
        for _ in 0..30 {
            p.record_script("always", 1.0);
            p.record_script("shadow", 0.0);
            p.end_frame();
        }
        // A script spawned thirty frames in — a prefab, a scene load, a pickup.
        for _ in 0..30 {
            p.record_script("always", 1.0);
            p.record_script("shadow", 4.0);
            p.record_script("latecomer", 4.0);
            p.end_frame();
        }
        let late = p.script("latecomer").expect("on").ms;
        let shadow = p.script("shadow").expect("on").ms;
        assert!(
            (late - shadow).abs() < shadow.max(1e-6) * 0.02,
            "a latecomer reads {late} where the same cost known all along reads {shadow}"
        );
    }

    /// The accounted total is the buckets added up, and it is not claimed to be
    /// the frame time.
    #[test]
    fn the_accounted_total_is_the_sum_of_the_buckets() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.record(Bucket::Scripts, 2.0);
        p.record(Bucket::Render, 4.0);
        p.end_frame();
        let sum: f32 = Bucket::ALL.iter().filter_map(|b| p.bucket(*b)).map(|c| c.ms).sum();
        assert!((p.accounted_ms().unwrap() - sum).abs() < 1e-4);
        assert!((p.accounted_ms().unwrap() - 6.0).abs() < 1e-3);
    }
}

#[cfg(test)]
mod last_parts_tests {
    use super::*;

    /// **A part is readable every frame, not only in a hitch.** A game
    /// watching how much of scatter was settling got the number only on frames
    /// slow enough to be hitches.
    #[test]
    fn the_last_frames_parts_are_kept() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.record(Bucket::Scatter, 1.5);
        p.record_part("scatterSettle", 0.9);
        p.end_frame();
        assert_eq!(p.last_parts(), &[("scatterSettle", 0.9)]);
        assert!(p.take_hitches().is_empty(), "1.5 ms is no hitch, so only last_parts could carry it");
        p.end_frame();
        assert!(p.last_parts().is_empty(), "a frame that settled nothing reports nothing");
    }
}

#[cfg(test)]
mod frame_record_tests {
    use super::*;

    /// **Every frame is kept exactly, not smoothed.** A 40 ms physics frame
    /// between 2 ms ones reads 40 in its own record, with its parts, its
    /// scripts and the engine calls they made, where the mean reads a few;
    /// the ring holds [`FRAME_RING`] and drops the oldest.
    #[test]
    fn each_frame_is_kept_exactly_with_its_parts_scripts_and_calls() {
        let mut p = FrameProfile::default();
        p.enable(true);
        for _ in 0..10 {
            p.record(Bucket::Physics, 2.0);
            p.end_frame();
        }
        p.record(Bucket::Physics, 40.0);
        p.record_part("physicsPairs", 30.0);
        p.record_script("planet_camera", 27.0);
        p.record_call("planet_camera", "spherecast", 12, 25.0);
        p.record_body("Plane", 31.0);
        p.end_frame();
        let last = p.recent_frames(1).next().expect("a frame").clone();
        assert_eq!(last.frame, 10);
        assert_eq!(last.bucket(Bucket::Physics), 40.0, "{last:?}");
        assert!(p.bucket(Bucket::Physics).unwrap().ms < 10.0, "the mean is what it was");
        assert_eq!(last.part("physicsPairs"), 30.0);
        assert_eq!(last.scripts, vec![("planet_camera".to_string(), 27.0)]);
        assert_eq!(last.calls, vec![EngineCall { script: "planet_camera".into(), call: "spherecast", n: 12, ms: 25.0 }]);
        assert_eq!(last.bodies, vec![("Plane".to_string(), 31.0)]);
        assert_eq!(p.last_calls().len(), 1);
        // The frame before is still the 2 ms one.
        let two: Vec<_> = p.recent_frames(2).collect();
        assert_eq!(two[0].bucket(Bucket::Physics), 2.0);
        for _ in 0..FRAME_RING + 5 {
            p.end_frame();
        }
        assert_eq!(p.recent_frames(10_000).count(), FRAME_RING);
        assert!(p.last_calls().is_empty(), "a frame that called nothing reports nothing");
    }

    /// **The hitch threshold is the game's.** A 40 ms frame is no hitch at
    /// the default 50 and is one at 33, with its scripts and samples.
    #[test]
    fn a_game_sets_its_own_hitch_threshold() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.end_frame();
        p.record(Bucket::Scripts, 40.0);
        p.end_frame();
        assert!(p.take_hitches().is_empty(), "40 ms is under the default {HITCH_MS}");
        p.set_hitch_threshold(33.0);
        assert_eq!(p.hitch_threshold(), 33.0);
        p.set_sampling(Some(0.25));
        p.record(Bucket::Scripts, 40.0);
        p.record_script("vessel", 38.0);
        p.record_sample("vessel", "solve", 120, 35.0);
        p.record_sample("vessel", "solve", 120, 1.0);
        p.record_sample("vessel", "update", 9, 2.0);
        p.end_frame();
        let h = p.take_hitches();
        assert_eq!(h.len(), 1, "{h:?}");
        assert_eq!(h[0].scripts[0].0, "vessel");
        assert_eq!((h[0].samples[0].func.as_str(), h[0].samples[0].line, h[0].samples[0].n), ("solve", 120, 2));
        p.set_hitch_threshold(0.0);
        assert_eq!(p.hitch_threshold(), 1.0, "a zero threshold would make every frame a hitch");
        // The totals since the last read, hottest first, then empty.
        let s = p.take_samples();
        assert_eq!((s[0].line, s[0].ms), (120, 36.0));
        assert!(p.take_samples().is_empty());
    }

    /// **Untimed says what it was where the engine knows.** A named wait is
    /// counted into `untimed` once, not on top of the rest of the gap, and a
    /// sample list cannot grow past its cap.
    #[test]
    fn a_named_wait_is_part_of_untimed_and_not_added_twice() {
        let mut p = FrameProfile::default();
        p.enable(true);
        let outer = crate::time::Instant::now();
        p.end_frame();
        p.record_wait("frameCap", 5.0);
        p.record(Bucket::Render, 1.0);
        // A frame longer than what was recorded, so there is a remainder.
        while outer.elapsed().as_secs_f32() < 0.010 {}
        p.end_frame();
        let wall = outer.elapsed().as_secs_f32() * 1000.0;
        let r = p.recent_frames(1).next().unwrap().clone();
        assert_eq!(r.part("frameCap"), 5.0);
        // The frame is its wall time, not the wall time plus the wait again.
        assert!(r.ms <= wall + 0.5, "a {wall:.2} ms frame added up to {:.2}: {r:?}", r.ms);
        assert!(r.bucket(Bucket::Other) >= 5.0, "{r:?}");
        let mut list = Vec::new();
        for i in 0..MAX_SAMPLE_LINES as u32 + 50 {
            add_sample(&mut list, "s", "f", i, 1, 1.0);
        }
        assert_eq!(list.len(), MAX_SAMPLE_LINES);
        assert_eq!(list.last().unwrap().script, "(other)");
    }

    /// **What held a frame.** Mostly waiting, about one refresh: the display.
    /// Mostly waiting, three refreshes with a GPU that finished in 2 ms: a
    /// compositor (a hidden window paced at 20 fps). The GPU's own time
    /// filling the frame: the GPU. Mostly work: nothing held it.
    #[test]
    fn a_frame_mostly_spent_waiting_names_what_held_it() {
        let r = 1000.0 / 60.0;
        assert_eq!(pacing_cause(16.7, 15.0, 0.0, Some(3.0), r, true), Some("display"));
        assert_eq!(pacing_cause(50.0, 48.0, 0.0, Some(2.0), r, true), Some("compositor"));
        assert_eq!(pacing_cause(50.0, 48.0, 0.0, None, r, true), Some("compositor"));
        assert_eq!(pacing_cause(50.0, 40.0, 0.0, Some(45.0), r, true), Some("gpu"));
        assert_eq!(pacing_cause(50.0, 5.0, 0.0, Some(3.0), r, true), None);
        assert_eq!(pacing_cause(33.0, 1.0, 25.0, None, r, false), Some("frameCap"));
        assert_eq!(pacing_cause(20.0, 15.0, 0.0, None, r, false), Some("gpu"));
        let mut p = FrameProfile::default();
        p.enable(true);
        p.set_pacing_setup(PacingSetup { present_mode: "fifo", vsync: true, refresh_hz: 60.0, frame_cap: 0.0 });
        p.end_frame();
        p.set_gpu(Vec::new(), 2.0);
        for _ in 0..10 {
            p.record(Bucket::GpuWait, 48.0);
            p.record(Bucket::Render, 2.0);
            p.end_frame();
        }
        let pace = p.pacing();
        assert_eq!(pace.limited_by, Some("compositor"), "{pace:?}");
        assert_eq!(pace.setup.present_mode, "fifo");
        assert!(pace.paced_frames >= 10, "{pace:?}");
        assert_eq!(p.recent_frames(1).next().unwrap().paced, Some("compositor"));
    }
}
