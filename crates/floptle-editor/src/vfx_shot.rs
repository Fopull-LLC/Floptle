//! `floptle vfx` — **what does this effect actually look like?**
//!
//! Renders one particle effect to PNGs with no window, at several moments along
//! its own timeline, through the editor's own offscreen path.
//!
//! ## The gap it closes
//!
//! Everything else about an effect can be read: the `.vfx.ron` says its tracks,
//! its curves, its emission rates. None of that says what it *looks* like, and a
//! particle effect is nothing but what it looks like. Somebody working without
//! the editor open — an automated caller, or anybody on a machine with no
//! display — had no way to see one at all, so the loop was: change a number,
//! guess, change it again. This is the verb that turns guessing into looking.
//!
//! ## Several moments, one camera
//!
//! An effect is a thing that happens over time, so a single frame is the wrong
//! question: a burst reads as a blank frame before it fires and as smoke after
//! it, and both are correct. So this renders a spread of moments across the
//! effect's own span — and reports the seconds each one is at, so the next run
//! can ask for a moment between two of them with `--at`.
//!
//! **The camera is the same in every frame.** It is framed once, on the union of
//! every moment being rendered, and then held. A camera re-framed per frame
//! would rescale the effect between pictures, and comparing "the start" against
//! "the middle" is the entire point — two frames at different zooms cannot be
//! compared at all, and nothing in the picture would say they were. The cost is
//! that no single frame fills the sheet the way it would on its own; `--at` on
//! one moment is the close look.
//!
//! ## Each moment is a scrub from zero
//!
//! Every frame is an independent [`EffectInstance::simulate_to_at`] from `t = 0`
//! — the sim is deterministic and seeded, so `--at 0.5` on its own gives exactly
//! the frame `--frames` would put at 0.5s. That is what makes the contact sheet
//! and the single frame the same picture, and it is the reason the middle of an
//! effect can be looked at without watching the start of it first.
//!
//! ## Which moments, decided by looking
//!
//! Where the effect is *visible* is measured, not computed — see
//! [`visible_window`]. Every version of this that reasoned from the simulation
//! needed a threshold, and each threshold was right for one effect and wrong for
//! the next: a burst whose particles live a seventh of a second got four of its
//! five frames after the sparks had gone out, and an explosion's smoke stayed
//! "alive" for a fifth of a second past the last frame anything could be seen
//! in. The effect is rendered at thumbnail size across its whole timeline first,
//! and the frames are spread over the part where something lands in the picture.
//!
//! ## An empty frame is said out loud
//!
//! A frame the effect does not reach looks exactly like an effect that does not
//! work, a texture that did not load, and a camera pointing the wrong way. So
//! every frame reports how much of the picture the effect changed — measured
//! against the same view rendered without it, so the number cannot disagree with
//! the image beside it — and frames with nothing in them are named. This engine
//! has paid for silent nothing often enough.

use std::path::{Path, PathBuf};

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_render::{Gpu, Projection, RenderCamera};
use floptle_vfx::EffectInstance;

/// One rendered moment.
pub(crate) struct Frame {
    /// Effect time, in seconds.
    pub t: f32,
    /// The share of the picture the effect actually covers, 0..1.
    ///
    /// **Measured off the rendered pixels, not off the simulation.** Every count
    /// derived from the sim can disagree with the image beside it — a particle
    /// at half a percent alpha is present in one and absent in the other — and a
    /// number that contradicts its own picture is worse than no number. The
    /// pixels are already in hand by the time this is filled in, so this costs
    /// a pass over them and cannot be wrong.
    pub coverage: f32,
    pub path: PathBuf,
}

/// The latest moment anything could still be alive, from the asset alone.
///
/// A **ceiling**, not the answer — [`measure`] finds the answer by running the
/// effect. This exists to bound that run: an upper bound computed from the file
/// is cheap and cannot be wrong in the direction that matters, where a formula
/// asked to be exact very much can. It was: `lifetime + the longest clip` reads
/// a 20-particle burst over `[0, 0.15]` on a 0.9s timeline as 1.1 seconds long,
/// which put four of five frames after every spark had gone out.
///
/// A particle born by a clip lives that clip's length, so the last one a clip
/// can produce dies at `end + (end - start)`, widened by the clip's own lifetime
/// jitter. A looping effect has no end at all, so its ceiling is one lifetime:
/// past that it repeats, and photographing the repeat says nothing new.
pub(crate) fn span_ceiling(doc: &floptle_scene::VfxEffectDoc) -> f32 {
    let lifetime = doc.lifetime.max(1e-3);
    match doc.playback {
        floptle_scene::VfxPlaybackDoc::Looping => lifetime,
        floptle_scene::VfxPlaybackDoc::OneShot => doc
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(|c| c.end + (c.end - c.start).max(1e-3) * (1.0 + c.lifetime_jitter.max(0.0)))
            .fold(lifetime, f32::max),
    }
}

/// How much light this moment puts on the screen, and where it is.
///
/// Returns a weight — opacity times a particle's own area, summed — and feeds
/// every contributing particle's position and size to `each` for the framing.
///
/// **Not `alive()`.** A particle whose colour curve has reached zero alpha, or
/// whose size curve has reached zero, is still alive and still costs a slot —
/// an explosion's smoke stays alive for a full second after it has faded out of
/// sight. Counting those put "15 particles" beside a picture with nothing in it,
/// which is the exact shape of answer this verb exists to stop giving.
fn weigh(inst: &EffectInstance, mut each: impl FnMut(Vec3, f32)) -> f32 {
    /// One step of 8-bit alpha. Below this a particle is not merely faint, it is
    /// arithmetically absent.
    const MIN_ALPHA: f32 = 1.0 / 255.0;
    let mut w = 0.0;
    for track in 0..inst.effect.tracks.len() {
        inst.sample_track(track, |s| {
            if s.color[3] > MIN_ALPHA && s.size > 0.0 {
                w += s.color[3] * s.size * s.size;
                each(s.pos, s.size);
            }
        });
    }
    w
}

/// When the effect is actually visible: the first moment anything is alive and
/// the last, found by running it.
///
/// **Measured rather than derived.** What an author wants photographed is the
/// part where something is happening, and the only thing that knows where that
/// is, is the simulation — clip kinds, pulses, jitter and per-track lifetimes
/// all move it, and every one of them is a way for an arithmetic answer to be
/// confidently wrong. Running it costs a few hundred steps of a sim built to be
/// scrubbed.
///
/// `None` when nothing is ever visible inside the ceiling: an effect that emits
/// nothing — or emits only fully transparent particles — has no visible span,
/// and picking one for it would put five pictures of an empty stage in front of
/// somebody.
pub(crate) fn measure(
    compiled: &std::sync::Arc<floptle_vfx::CompiledEffect>,
    ceiling: f32,
    emitter: floptle_core::transform::Transform,
) -> Option<(f32, f32)> {
    /// How faint, against the effect's own strongest moment, still counts as
    /// part of it.
    ///
    /// **Relative, not an absolute alpha.** An absolute floor has to be a guess
    /// about perception, and every guess is wrong for some effect: a particle at
    /// half a percent alpha is arithmetically present and invisible, which is
    /// how an explosion's span ran a fifth of a second past the last frame
    /// anything could be seen in. Measuring against the effect's own peak needs
    /// no such guess and works the same for a flashbulb and for a wisp.
    const FLOOR: f32 = 0.02;

    let mut inst = EffectInstance::new(std::sync::Arc::clone(compiled), 1);
    let mut weights: Vec<(f32, f32)> = Vec::new();
    let mut t = 0.0f32;
    // The sim's own scrub step, so what this measures is what a scrub to the
    // same moment will show rather than something a coarser walk stepped over.
    while t < ceiling {
        inst.advance_at(floptle_vfx::SCRUB_STEP, crate::vfx::VFX_GRAVITY, emitter);
        t += floptle_vfx::SCRUB_STEP;
        weights.push((t, weigh(&inst, |_, _| {})));
    }
    let peak = weights.iter().map(|&(_, w)| w).fold(0.0f32, f32::max);
    if peak <= 0.0 {
        return None;
    }
    let cut = peak * FLOOR;
    let first = weights.iter().find(|&&(_, w)| w >= cut)?.0;
    let last = weights.iter().rev().find(|&&(_, w)| w >= cut)?.0;
    Some((first, last.max(first)))
}

/// The window the effect is actually visible in, decided by looking at it.
///
/// **The only non-arbitrary answer.** Every version of this that reasoned from
/// the simulation needed a threshold — a particle at half a percent alpha is
/// arithmetically present and invisible — and each threshold was right for one
/// effect and wrong for the next. An explosion's smoke stays "alive" for a
/// second after it has faded out, and stays above any relative weight floor for
/// most of that, so the last frame of a five-frame sheet was reliably a picture
/// of nothing.
///
/// This renders the effect at thumbnail size across its whole ceiling and keeps
/// the moments where something lands in the frame. That is the same question the
/// verb is answering anyway, asked cheaply: [`PROBES`] renders at 64 x 64,
/// against a project load that has already happened.
///
/// `None` when no probe sees anything — an effect too small to cover a pixel at
/// this size, most likely — and the caller falls back to the sim-side estimate.
fn visible_window(
    ed: &mut crate::Editor,
    cam: &RenderCamera,
    compiled: &std::sync::Arc<floptle_vfx::CompiledEffect>,
    host: floptle_core::Entity,
    key: &str,
    ceiling: f32,
    emitter: floptle_core::transform::Transform,
) -> Option<(f32, f32)> {
    /// How many moments to look at. Enough to place the ends to a few percent of
    /// the span, few enough that the whole pass is lost in the noise of opening
    /// the project.
    const PROBES: usize = 32;
    const SIZE: u32 = 64;

    let baseline = baseline_frame(ed, cam, host, SIZE, SIZE)?;
    let (mut first, mut last) = (None, None);
    for i in 0..PROBES {
        // Skipping `t = 0`: the sim fires a burst on its FIRST step, so the
        // zeroth probe is empty for every effect there is and would only ever
        // be the one that fails.
        let t = ceiling * (i + 1) as f32 / PROBES as f32;
        let mut inst = EffectInstance::new(std::sync::Arc::clone(compiled), 1);
        inst.simulate_to_at(t, crate::vfx::VFX_GRAVITY, emitter);
        ed.vfx.instances.insert(host, (key.to_string(), inst));
        let px = crate::shot::render_frame_pixels(ed, cam, SIZE, SIZE, u32::MAX)?;
        if coverage_against(&px, &baseline) > 0.0 {
            first.get_or_insert(t);
            last = Some(t);
        }
    }
    ed.vfx.instances.remove(&host);
    Some((first?, last?))
}

/// The moments to render when the caller did not name any.
///
/// Evenly spaced across `[from, to]`, **ends included**: "the start" and "the
/// end" are the two an author asks for by name, and a spread that stopped short
/// of either would answer neither. One frame means the middle, which is the
/// single most informative moment of an effect nobody has said anything else
/// about.
///
/// The ends are the effect's *visible* ends, not zero and the timeline's length.
/// `t = 0` is always empty — the sim fires a burst on its first step, not before
/// it — so a spread anchored there spends its most important frame on a picture
/// of nothing.
pub(crate) fn spread(from: f32, to: f32, frames: usize) -> Vec<f32> {
    match frames {
        0 => Vec::new(),
        1 => vec![(from + to) * 0.5],
        n => (0..n).map(|i| from + (to - from) * i as f32 / (n - 1) as f32).collect(),
    }
}

/// Parse `--at` as a comma- or space-separated list of seconds.
///
/// Negative is a typo rather than a request — an effect has no time before zero —
/// and saying so beats rendering `t = 0` and letting the caller conclude that
/// their number did something.
pub(crate) fn parse_times(s: &str) -> Result<Vec<f32>, String> {
    let mut out = Vec::new();
    for piece in s.split([',', ' ']).map(str::trim).filter(|p| !p.is_empty()) {
        match piece.parse::<f32>() {
            Ok(t) if t.is_finite() && t >= 0.0 => out.push(t),
            Ok(t) => {
                return Err(format!("--at {t} is not a moment in an effect — times start at 0"));
            }
            Err(_) => {
                return Err(format!(
                    "--at wants seconds, comma-separated (say 0,0.25,0.5), not {piece:?}"
                ));
            }
        }
    }
    if out.is_empty() {
        return Err("--at named no times".into());
    }
    Ok(out)
}

/// Parse `--background` as `#rrggbb` (the `#` optional).
pub(crate) fn parse_color(s: &str) -> Result<[f32; 3], String> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("--background wants a hex colour (say 202024), not {s:?}"));
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f32 / 255.0;
    Ok([byte(0), byte(2), byte(4)])
}

/// A file name for one moment, with the time in it.
///
/// The time is in the name because the frames are looked at as files — a
/// directory listing that says which is which beats one of `0.png … 4.png` and a
/// mapping the caller has to keep. Milliseconds, zero-padded, so they sort in
/// timeline order rather than lexicographically shuffled.
pub(crate) fn frame_name(effect: &str, t: f32) -> String {
    format!("{effect}.t{:05}ms.png", millis(t))
}

/// The moment, in whole milliseconds — the one rounding both the file name and
/// the reported time come from.
///
/// **One rounding, or they disagree.** Printing `{:.3}` of the seconds and
/// rounding the milliseconds separately puts `t = 0.112s` next to
/// `…t00113ms.png` on the same line, because the two round a half differently.
/// It is one millisecond and it is cosmetic, and it still reads as a tool that
/// cannot keep its own two numbers straight.
pub(crate) fn millis(t: f32) -> u64 {
    (t * 1000.0).round().max(0.0) as u64
}

/// The last path segment of an effect key — what the files get named after.
fn stem_of(key: &str) -> String {
    key.rsplit('/').next().unwrap_or(key).to_string()
}

/// The box everything sampled fits inside: its centre, and its half-extents.
///
/// `None` when there is nothing to look at, which is a real answer — an effect
/// that emits nothing at any sampled moment has no framing, and inventing one
/// would put a confident picture of empty space in front of somebody.
fn bounds_of(samples: &[(Vec3, f32)]) -> Option<(Vec3, Vec3)> {
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for &(p, size) in samples {
        // The particle's extent, not its centre: a single large billboard has a
        // degenerate point cloud and a real size, and framing on the points
        // alone would put the camera inside it.
        let r = Vec3::splat(size.max(0.0) * 0.5);
        lo = lo.min(p - r);
        hi = hi.max(p + r);
    }
    if !lo.is_finite() || !hi.is_finite() {
        return None;
    }
    // A floor on the size: an effect whose particles have not moved yet is a
    // point, and a camera fitted to a point is a camera at the origin.
    Some(((lo + hi) * 0.5, ((hi - lo) * 0.5).max(Vec3::splat(0.125))))
}

/// Everything one run needs. One struct rather than eleven arguments, because
/// they arrive together from one command line and travel together to one render.
pub(crate) struct Args<'a> {
    pub root: &'a Path,
    pub effect: &'a str,
    pub scene: Option<&'a str>,
    pub camera: Option<&'a str>,
    /// Explicit moments, or `None` to spread `frames` across the effect's span.
    pub at: Option<Vec<f32>>,
    pub frames: usize,
    pub size: (u32, u32),
    pub background: Option<[f32; 3]>,
    pub out: Option<PathBuf>,
    pub json: bool,
}

/// Run the verb. Returns the process exit code.
pub(crate) fn run(args: Args<'_>) -> i32 {
    let Args { root, effect, scene, camera, at, frames, size, background, out, json } = args;
    if !root.join("project.ron").is_file() {
        eprintln!("{} is not a project directory (no project.ron)", root.display());
        return 2;
    }
    let (w, h) = (size.0.max(1), size.1.max(1));

    // The GPU first, then the project — the same order `shot` uses and for the
    // same reason: `open_project` imports models and adopts paint, and both look
    // for a device.
    let gpu = Gpu::headless_hdr(w, h);
    gpu.device.on_uncaptured_error(std::sync::Arc::new(|e: wgpu::Error| {
        eprintln!(
            "this machine's graphics driver could not build the renderer, so there is no \
             picture to write:\n  {e}"
        );
        eprintln!(
            "if this machine has only an OpenGL adapter, that is the likely cause: floptle's \
             shaders need Vulkan, Metal or DirectX 12."
        );
        std::process::exit(1);
    }));
    let mut ed = crate::Editor {
        show_gizmos: false,
        console: crate::console::ConsoleState { mirror_to_stderr: true, ..Default::default() },
        ..Default::default()
    };
    ed.attach_gpu(gpu);
    ed.open_project(root.to_path_buf());

    // **A stage, or the level.** With no `--scene` the effect is rendered on its
    // own — that is the question being asked while an effect is being authored,
    // and it is also the only one answerable in a project whose scenes are not
    // ready yet. `--scene` is the follow-up question ("how does it read in the
    // room it plays in"), and it answers it in the room.
    if let Some(s) = scene {
        let Some(path) = crate::inspect::resolve_scene(root, s) else {
            eprintln!("no scene called {s} under {}", root.join("scenes").display());
            return 1;
        };
        ed.open_scene_file(&path.to_string_lossy());
    } else {
        ed.world = floptle_core::World::new();
        stage(&mut ed);
    }
    if let Some(rgb) = background {
        set_background(&mut ed, rgb);
    }

    let Some(doc) = ed.vfx.doc(effect).cloned() else {
        eprintln!("no effect called {effect} in {}", root.display());
        let known: Vec<&str> = ed.vfx.effects.iter().map(|(k, _)| k.as_str()).collect();
        if known.is_empty() {
            eprintln!("this project has no .vfx.ron effects in it at all");
        } else {
            eprintln!("this project has: {}", known.join(", "));
        }
        return 1;
    };
    let Some(compiled) = ed.vfx.effect(effect) else { return 1 };

    // The emitter. In a scene, the node that carries this effect — so World-space
    // tracks anchor where the game plays them, exactly as the Particles tab
    // previews them. On the bare stage, the origin.
    let anchor = scene.and(anchor_for(&ed, effect));
    let emitter = anchor
        .map(|e| floptle_core::world_transform(&ed.world, e))
        .unwrap_or(floptle_core::transform::Transform::IDENTITY);

    let ceiling = span_ceiling(&doc);

    // The node the gather draws through. `VfxSystem::collect` walks `instances`
    // by entity and asks the world for each one's transform, so the instance
    // needs a node even on the bare stage — and putting it on a real node is
    // what keeps this the same path the running game draws through rather than
    // a fourth one.
    let host = anchor.unwrap_or_else(|| {
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::transform::Transform::IDENTITY);
        ed.world.insert(
            e,
            floptle_core::ParticleSystem { asset: effect.to_string(), play_on_start: true },
        );
        e
    });
    // Particle textures and mesh-track models have to be on the GPU before the
    // gather resolves a batch's texture — the windowed editor does this at the
    // top of every frame and this verb has no frame. Without it every textured
    // billboard draws untextured, which is a picture that still looks like a
    // picture. Before the probe pass below, which is a render like any other.
    ed.ensure_vfx_assets();
    ed.refresh_gi();
    if scene.is_some() {
        ed.sync_map_meshes();
        ed.sync_map_paint();
        ed.sync_terrain_gpu();
    }

    // A scene render looks through the scene's camera, and a scene without one
    // has no view at all — settled here, before anything is rendered, so the
    // probe pass and the frames cannot disagree about where they are looking
    // from.
    let scene_cam = match scene {
        None => None,
        Some(_) => match scene_camera(&ed, camera) {
            Some(c) => Some(c),
            None => {
                match camera {
                    Some(name) => eprintln!("this scene has no camera called {name}"),
                    None => eprintln!(
                        "this scene has no camera, so there is no view to render it in"
                    ),
                }
                return 1;
            }
        },
    };

    // **When is this effect visible?** Asked of the picture rather than of the
    // simulation — see `visible_window`. The provisional camera it is asked
    // through is fitted to everything the effect ever does, so nothing can fall
    // outside the frame and be missed; the real camera is fitted afterwards, to
    // the moments that turn out to be worth photographing.
    let window = match at {
        // Moments the caller named are rendered as asked, empty or not: somebody
        // who types `--at 3` is asking what is there at three seconds, and
        // answering with a different moment would answer a different question.
        // No probe pass either — it exists to choose moments, and they are chosen.
        Some(_) => None,
        None => {
            let mut everything: Vec<(Vec3, f32)> = Vec::new();
            let mut t = 0.0f32;
            let mut walk = EffectInstance::new(std::sync::Arc::clone(&compiled), 1);
            while t < ceiling {
                walk.advance_at(floptle_vfx::SCRUB_STEP, crate::vfx::VFX_GRAVITY, emitter);
                t += floptle_vfx::SCRUB_STEP;
                weigh(&walk, |pos, size| everything.push((pos, size)));
            }
            let wide = scene_cam.unwrap_or_else(|| {
                fitted_camera(&everything, emitter.translation, w as f32 / h as f32)
            });
            visible_window(&mut ed, &wide, &compiled, host, effect, ceiling, emitter)
                // A probe that saw nothing is not proof there is nothing: an
                // effect can be too small to cover a pixel at 64 x 64. Fall back
                // to what the simulation says rather than refusing.
                .or_else(|| measure(&compiled, ceiling, emitter))
        }
    };
    let times = match at {
        Some(t) => t,
        None => match window {
            Some((from, to)) => spread(from, to, frames.max(1)),
            None => {
                eprintln!(
                    "{effect} never puts anything on the screen in the {ceiling:.3}s its \
                     timeline and clips cover, so there is nothing to photograph. Check the \
                     effect has a clip with an emit on it."
                );
                return 1;
            }
        },
    };
    if times.is_empty() {
        eprintln!("--frames 0 asks for no pictures");
        return 2;
    }
    let span = window.map(|(_, to)| to).unwrap_or(ceiling);

    // **Frame the camera once, on every moment at once.** Sampling all of them
    // before rendering any is the whole reason this is two passes: a camera
    // fitted per frame would rescale the effect between pictures, and the frames
    // exist to be compared against each other.
    let mut cloud: Vec<(Vec3, f32)> = Vec::new();
    let mut weight_at: Vec<f32> = Vec::with_capacity(times.len());
    for &t in &times {
        let mut inst = EffectInstance::new(std::sync::Arc::clone(&compiled), 1);
        inst.simulate_to_at(t, crate::vfx::VFX_GRAVITY, emitter);
        weight_at.push(weigh(&inst, |pos, size| cloud.push((pos, size))));
    }
    let total: f32 = weight_at.iter().sum();
    if total <= 0.0 {
        // Only reachable through `--at`: a spread is measured off the moments
        // something IS visible, so it cannot land entirely in the gaps.
        eprintln!(
            "{effect} shows nothing at any of the {} moment(s) asked for — the pictures would \
             all be an empty stage. Drop --at and it will find the moments the effect is \
             actually visible in.",
            times.len()
        );
        return 1;
    }

    let cam = match scene_cam {
        // A scene has its own view, and that view is the answer to "how does it
        // read in the room" — replacing it with one fitted to the particles
        // would answer a different question.
        Some(c) => c,
        None => fitted_camera(&cloud, emitter.translation, w as f32 / h as f32),
    };

    let dir = out.unwrap_or_else(|| root.to_path_buf());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("could not create {}: {e}", dir.display());
        return 1;
    }
    let stem = stem_of(effect);
    // The same view with the effect switched off, so each frame can be measured
    // against what was already there rather than against its own background.
    let Some(baseline) = baseline_frame(&mut ed, &cam, host, w, h) else {
        eprintln!("no GPU: this machine has no adapter floptle can render on");
        return 1;
    };
    let mut written: Vec<Frame> = Vec::new();
    let mut pixels: Vec<Vec<u8>> = Vec::new();
    for &t in &times {
        let mut inst = EffectInstance::new(std::sync::Arc::clone(&compiled), 1);
        inst.simulate_to_at(t, crate::vfx::VFX_GRAVITY, emitter);
        ed.vfx.instances.insert(host, (effect.to_string(), inst));

        let Some(px) = crate::shot::render_frame_pixels(&mut ed, &cam, w, h, u32::MAX) else {
            eprintln!("no GPU: this machine has no adapter floptle can render on");
            return 1;
        };
        let path = dir.join(frame_name(&stem, t));
        let Some(buf) = image::RgbaImage::from_raw(w, h, px.clone()) else {
            eprintln!("the render came back the wrong size");
            return 1;
        };
        if let Err(e) = buf.save(&path) {
            eprintln!("could not write {}: {e}", path.display());
            return 1;
        }
        let coverage = coverage_against(&px, &baseline);
        pixels.push(px);
        written.push(Frame { t, coverage, path });
    }

    // **Say when the pictures have nothing in them.** A frame the effect does
    // not reach is a real answer — but it looks exactly like a broken renderer,
    // a missing texture and a camera pointing the wrong way, and somebody who
    // cannot tell those apart has learnt nothing from running this.
    let blank = written.iter().filter(|f| f.coverage <= 0.0).count();
    if blank == written.len() {
        match scene {
            Some(name) => eprintln!(
                "{effect} changes nothing in any of these frames. It was rendered where {name} \
                 puts it, through that scene's camera — most likely that camera is not looking \
                 at it. Drop --scene to see the effect on its own."
            ),
            None => eprintln!(
                "{effect} changes nothing in any of these frames — every picture is the empty \
                 stage. That is the effect, not the renderer."
            ),
        }
    } else if blank > 0 {
        eprintln!(
            "note: {blank} of {} frames have nothing in them — see the percentages above for \
             which.",
            written.len()
        );
    }

    // **One picture of the whole effect**, laid out in timeline order. A caller
    // looking at five files pays five reads to answer one question; the sheet
    // answers it in one, and the individual frames are still there for the
    // moment that turns out to be the interesting one.
    let sheet = (written.len() > 1).then(|| dir.join(format!("{stem}.sheet.png")));
    if let Some(path) = &sheet {
        let img = contact_sheet(&pixels, w, h, background.unwrap_or(DEFAULT_BACKGROUND));
        if let Err(e) = img.save(path) {
            eprintln!("could not write {}: {e}", path.display());
            return 1;
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "effect": effect,
                "span": span,
                "sheet": sheet.as_ref().map(|p| p.to_string_lossy()),
                "frames": written
                    .iter()
                    .map(|f| serde_json::json!({
                        "t": f.t,
                        "coverage": f.coverage,
                        "path": f.path.to_string_lossy(),
                    }))
                    .collect::<Vec<_>>(),
            })
        );
    } else {
        println!("{effect}: visible over {:.3}s, {} frame(s) at {w}x{h}", span, written.len());
        for f in &written {
            println!(
                "  t={:>7.3}s  {:>5.1}% of frame  {}",
                millis(f.t) as f64 / 1000.0,
                f.coverage * 100.0,
                f.path.display()
            );
        }
        if let Some(p) = &sheet {
            println!("  all of them: {}", p.display());
        }
    }
    0
}

/// How much of the frame the effect changed, against the same view without it.
///
/// **Against a baseline render, not against the background colour.** On the bare
/// stage those are the same thing; in a scene they are not remotely — most of a
/// level's frame is level, so "how much of this picture is not the commonest
/// colour in it" answered 24% for an effect, for the scene it was in, and for
/// the same scene with the effect switched off. A number that does not change
/// when the thing it measures is removed is not measuring it.
///
/// It also decides which moments are worth photographing (see
/// [`visible_window`]), so getting this wrong in a scene meant the probe pass
/// found the whole timeline "visible" — because the room was.
fn coverage_against(px: &[u8], baseline: &[u8]) -> f32 {
    let (chunks, _) = px.as_chunks::<4>();
    let (base, _) = baseline.as_chunks::<4>();
    if chunks.is_empty() || chunks.len() != base.len() {
        return 0.0;
    }
    // Three levels: enough to ignore the dither and any frame-to-frame wobble
    // in the post chain, small enough that the faintest smoke anybody can see
    // still counts.
    let differs = chunks
        .iter()
        .zip(base)
        .filter(|(p, b)| {
            p[0].abs_diff(b[0]) > 3 || p[1].abs_diff(b[1]) > 3 || p[2].abs_diff(b[2]) > 3
        })
        .count();
    differs as f32 / chunks.len() as f32
}

/// The view with no effect in it, as pixels — what every frame is measured
/// against.
fn baseline_frame(
    ed: &mut crate::Editor,
    cam: &RenderCamera,
    host: floptle_core::Entity,
    w: u32,
    h: u32,
) -> Option<Vec<u8>> {
    ed.vfx.instances.remove(&host);
    crate::shot::render_frame_pixels(ed, cam, w, h, u32::MAX)
}

/// Tile the frames into one image, in timeline order, left to right then down.
///
/// A near-square grid rather than one row: a row of five 480-pixel frames is
/// 2400 pixels wide, and anything that displays it scales it down until the
/// particles are gone. The grid keeps each frame close to the size it was
/// rendered at.
fn contact_sheet(frames: &[Vec<u8>], w: u32, h: u32, background: [f32; 3]) -> image::RgbaImage {
    let n = frames.len().max(1);
    let cols = (n as f32).sqrt().ceil() as u32;
    let rows = (n as u32).div_ceil(cols.max(1));
    // **Filled, not left blank.** A grid rarely divides evenly, and a new
    // `RgbaImage` is transparent black — which anything showing the sheet paints
    // WHITE. So a five-frame sheet came back with a white block in the corner
    // that reads as a sixth frame, of a blown-out effect, that does not exist.
    let byte = |c: f32| (c.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
    let fill =
        image::Rgba([byte(background[0]), byte(background[1]), byte(background[2]), 255]);
    let mut out = image::RgbaImage::from_pixel(w * cols, h * rows, fill);
    for (i, px) in frames.iter().enumerate() {
        let (cx, cy) = (i as u32 % cols, i as u32 / cols);
        let Some(tile) = image::RgbaImage::from_raw(w, h, px.clone()) else { continue };
        image::imageops::replace(&mut out, &tile, (cx * w) as i64, (cy * h) as i64);
    }
    out
}

/// The node in the open scene carrying this effect, if one does.
fn anchor_for(ed: &crate::Editor, key: &str) -> Option<floptle_core::Entity> {
    let stem = key.rsplit('/').next().unwrap_or(key);
    ed.world
        .query::<floptle_core::ParticleSystem>()
        .find(|(_, ps)| ps.asset == key || ps.asset.rsplit('/').next() == Some(stem))
        .map(|(e, _)| e)
}

/// The camera a scene render looks through — the active one, or `--camera` by name.
fn scene_camera(ed: &crate::Editor, named: Option<&str>) -> Option<RenderCamera> {
    let mut fallback = None;
    for (e, m) in ed.world.query::<floptle_core::Matter>() {
        let floptle_core::Matter::Camera { fov_y, ortho, ortho_height, active, .. } = m else {
            continue;
        };
        let build = |e| {
            let wt = floptle_core::world_transform(&ed.world, e);
            RenderCamera::new(
                wt.translation,
                wt.rotation,
                Projection::of_camera(*fov_y, *ortho, *ortho_height, 0.05, 300_000.0),
            )
        };
        match named {
            Some(want) => {
                if ed.world.get::<floptle_core::Name>(e).is_some_and(|n| n.0 == want) {
                    return Some(build(e));
                }
            }
            None => {
                if *active {
                    return Some(build(e));
                }
                fallback.get_or_insert_with(|| build(e));
            }
        }
    }
    fallback
}

/// A camera placed to hold the whole effect, looking slightly down from the front.
///
/// Slightly down and slightly to one side rather than dead-on, because a
/// fountain seen from exactly level is a vertical line and a ring seen edge-on is
/// a horizontal one. Three-quarters is the angle a reference render is drawn at
/// for the same reason: it is the one that shows all three axes at once.
fn fitted_camera(cloud: &[(Vec3, f32)], origin: DVec3, aspect: f32) -> RenderCamera {
    const FOV_Y: f32 = 0.9;
    /// Room for the fades. A particle at the exact edge of the box has its own
    /// soft falloff outside it, and a frame with the effect flush against all
    /// four sides reads as one that cropped something.
    const MARGIN: f32 = 1.06;

    let (centre, half) = bounds_of(cloud).unwrap_or((Vec3::ZERO, Vec3::splat(1.0)));
    let centre = origin + centre.as_dvec3();
    let dir = Vec3::new(0.55, 0.35, 1.0).normalize();

    // Look back at the centre, with world up as the roll reference.
    //
    // **The basis has to be right-handed**, and the order of these two crosses
    // is the whole of that. `Y × forward` gets the sign backwards, which makes
    // `Mat3::from_cols` a determinant −1 matrix — and `Quat::from_mat3` of one
    // of those is not a rotation at all. It does not fail: it returns a
    // quaternion, the render succeeds, and the effect lands somewhere else in
    // the frame with nothing anywhere saying why.
    let fwd = -dir;
    let mut right = fwd.cross(Vec3::Y);
    if right.length_squared() < 1e-6 {
        // Looking straight up or down: world up is no longer a roll reference.
        right = Vec3::X;
    }
    let right = right.normalize();
    let up = right.cross(fwd);

    // **Fit the box, not the sphere around it.** A sphere is the easy answer and
    // it is √3 too big for a cube — the effect ends up filling a third of the
    // frame it was fitted to, and the rest of every picture is background. The
    // box's eight corners in the camera's own frame give the exact distance
    // instead: a corner at view depth `dist + d` fits when its sideways offset
    // is inside `(dist + d)·tan(fov/2)`, so each corner names a distance and the
    // furthest-out one wins.
    let tan_y = (FOV_Y * 0.5).tan();
    let tan_x = tan_y * aspect.max(1e-3);
    let mut dist = half.length() * 0.5;
    for i in 0..8 {
        let c = Vec3::new(
            if i & 1 == 0 { -half.x } else { half.x },
            if i & 2 == 0 { -half.y } else { half.y },
            if i & 4 == 0 { -half.z } else { half.z },
        ) * MARGIN;
        let d = c.dot(fwd);
        dist = dist.max(c.dot(right).abs() / tan_x - d);
        dist = dist.max(c.dot(up).abs() / tan_y - d);
    }
    // Never inside the thing being photographed, however the fit came out.
    let dist = dist.max(half.length() + 0.1);

    let eye = centre + (dir * dist).as_dvec3();
    // Columns are (right, up, BACK) — the camera looks down its own −Z.
    let rot = Quat::from_mat3(&floptle_core::math::Mat3::from_cols(right, up, -fwd));
    RenderCamera::new(eye, rot, Projection::of_camera(FOV_Y, false, 10.0, 0.01, 10_000.0))
}

/// The bare stage: lighting and a sky, and nothing else to look at.
///
/// Deliberately empty of geometry. Anything placed here to give a sense of scale
/// — a ground plane, a reference cube — is a thing the author did not put in
/// their effect, and every picture of every effect would carry it.
fn stage(ed: &mut crate::Editor) {
    let light = ed.world.spawn();
    ed.world.insert(
        light,
        floptle_core::Light {
            color: [1.0, 1.0, 1.0],
            ambient: [0.35, 0.36, 0.4],
            intensity: 1.4,
            direction: [0.4, -0.8, 0.35],
            shadows: false,
            ..Default::default()
        },
    );
    let sky = ed.world.spawn();
    ed.world.insert(sky, floptle_core::transform::Transform::IDENTITY);
    ed.world.insert(sky, floptle_core::Matter::default_skybox());
    set_background(ed, DEFAULT_BACKGROUND);
}

/// A neutral dark grey, not black.
///
/// **Black is the one background that lies about both blend modes at once**: an
/// additive spark is at its most flattering on it and an alpha smoke puff at its
/// least visible, so the same picture over-sells half an effect and hides the
/// other half. A mid-dark grey reads both honestly. `--background` overrides it
/// for the case where the effect is going to play somewhere specific.
const DEFAULT_BACKGROUND: [f32; 3] = [0.11, 0.11, 0.13];

/// Point the scene's sky at a flat colour.
fn set_background(ed: &mut crate::Editor, rgb: [f32; 3]) {
    let skies: Vec<floptle_core::Entity> = ed
        .world
        .query::<floptle_core::Matter>()
        .filter(|(_, m)| matches!(m, floptle_core::Matter::Skybox { .. }))
        .map(|(e, _)| e)
        .collect();
    for e in skies {
        if let Some(floptle_core::Matter::Skybox { color, texture, .. }) =
            ed.world.get_mut::<floptle_core::Matter>(e)
        {
            *color = rgb;
            // A textured sky would paint over the colour that was just asked
            // for, and the request would do nothing with nothing said.
            *texture = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The ends are moments too.** A spread that stopped short of either end
    /// would answer neither of the two questions an author actually asks by name
    /// — "what does the start look like" and "what does the end look like".
    #[test]
    fn a_spread_covers_the_whole_visible_span_ends_included() {
        assert_eq!(spread(0.0, 2.0, 5), vec![0.0, 0.5, 1.0, 1.5, 2.0]);
        assert_eq!(spread(0.0, 1.0, 2), vec![0.0, 1.0]);
        // One frame is the middle: the single most informative moment of an
        // effect nobody has said anything else about. Neither end is.
        assert_eq!(spread(0.0, 1.0, 1), vec![0.5]);
        assert!(spread(0.0, 1.0, 0).is_empty());
        // **It starts where the effect does, not at zero.** `t = 0` is always
        // empty — the sim fires a burst on its FIRST step, not before it — so a
        // spread anchored there spends its "start" frame on a picture of nothing.
        let s = spread(0.1, 0.5, 3);
        assert!(s[0] > 0.0, "the first frame must be a moment the effect is alive at: {s:?}");
        assert!((s[1] - 0.3).abs() < 1e-6, "and the middle is the middle: {s:?}");
        assert!((s[2] - 0.5).abs() < 1e-6);
    }

    /// **The ceiling has to cover a burst that outlives its own clip.**
    ///
    /// The formula this replaced was `lifetime + the longest clip`, and it read
    /// a 20-particle burst over `[0, 0.15]` on a 0.9s timeline as 1.1 seconds
    /// long. That is where the frames went: four of five landed after every
    /// spark had gone out, and the pictures were of an empty stage. It is also
    /// why the real span is measured rather than computed — this is only the
    /// bound the measurement runs inside.
    #[test]
    fn the_ceiling_is_a_bound_no_particle_outlives() {
        use floptle_scene::{VfxClipDoc, VfxPlaybackDoc};
        let mut doc = crate::vfx::starter_effect_doc("t");
        doc.lifetime = 0.9;
        doc.playback = VfxPlaybackDoc::OneShot;
        doc.tracks.truncate(1);
        // Sparks' own clip: a burst at 0 whose particles live 0.15s.
        doc.tracks[0].clips =
            vec![VfxClipDoc { start: 0.0, end: 0.15, lifetime_jitter: 0.0, emit: None }];
        // The timeline is the floor — a clip can sit anywhere inside it.
        assert!((span_ceiling(&doc) - 0.9).abs() < 1e-5, "{}", span_ceiling(&doc));

        // A clip that runs past the timeline pushes the ceiling out with it:
        // the last particle it can make is born at `end` and lives `end - start`.
        doc.tracks[0].clips =
            vec![VfxClipDoc { start: 0.0, end: 2.0, lifetime_jitter: 0.0, emit: None }];
        assert!((span_ceiling(&doc) - 4.0).abs() < 1e-5, "{}", span_ceiling(&doc));
        // …and jitter widens it, because a jittered particle lives longer.
        doc.tracks[0].clips =
            vec![VfxClipDoc { start: 0.0, end: 2.0, lifetime_jitter: 0.5, emit: None }];
        assert!((span_ceiling(&doc) - 5.0).abs() < 1e-5, "{}", span_ceiling(&doc));

        // A loop has no end to reach — one lifetime, and past that it repeats.
        doc.playback = VfxPlaybackDoc::Looping;
        assert!((span_ceiling(&doc) - 0.9).abs() < 1e-5);
    }

    /// The times reach the caller through the file names, so they have to be in
    /// them — and sort in timeline order rather than lexicographically shuffled.
    #[test]
    fn a_frame_is_named_for_the_moment_it_shows() {
        assert_eq!(frame_name("Sparks", 0.0), "Sparks.t00000ms.png");
        assert_eq!(frame_name("Sparks", 0.25), "Sparks.t00250ms.png");
        assert_eq!(frame_name("Sparks", 1.5), "Sparks.t01500ms.png");
        let mut names = [frame_name("f", 1.5), frame_name("f", 0.25), frame_name("f", 10.0)];
        names.sort();
        assert_eq!(names, [frame_name("f", 0.25), frame_name("f", 1.5), frame_name("f", 10.0)]);
        // **The name and the reported time round the same way.** Rounding the
        // milliseconds for one and formatting the seconds for the other put
        // `t = 0.112s` on the same line as `…t00113ms.png`.
        let t = 0.1125_f32;
        assert_eq!(
            format!("{:.3}", millis(t) as f64 / 1000.0),
            frame_name("f", t).trim_start_matches("f.t").trim_end_matches("ms.png")
                .trim_start_matches('0')
                .parse::<u64>()
                .map(|ms| format!("{:.3}", ms as f64 / 1000.0))
                .unwrap(),
            "the printed time and the file name disagree about which millisecond this is"
        );
    }

    #[test]
    fn times_are_seconds_and_a_typo_is_refused_by_name() {
        assert_eq!(parse_times("0,0.5,1"), Ok(vec![0.0, 0.5, 1.0]));
        assert_eq!(parse_times(" 0.25 "), Ok(vec![0.25]));
        assert_eq!(parse_times("0 0.5"), Ok(vec![0.0, 0.5]));
        // An effect has no time before it starts.
        assert!(parse_times("-1").is_err());
        // The refusal shows the shape, because whoever typed it cannot see the source.
        assert!(parse_times("start").unwrap_err().contains("0,0.25,0.5"));
        assert!(parse_times("").is_err());
    }

    #[test]
    fn a_background_is_a_hex_colour() {
        assert_eq!(parse_color("#ffffff"), Ok([1.0, 1.0, 1.0]));
        assert_eq!(parse_color("000000"), Ok([0.0, 0.0, 0.0]));
        assert!(parse_color("ff").is_err());
        assert!(parse_color("grey").unwrap_err().contains("202024"));
    }

    /// **The number beside a picture has to come from the picture, and it has to
    /// change when the effect is removed.**
    ///
    /// Two ways this went wrong, both of which shipped a number that looked
    /// fine. Counting live particles put "15 particles" next to an empty stage —
    /// an explosion's smoke stays alive for a fifth of a second after the last
    /// frame anything can be seen in. Counting pixels that differ from the
    /// frame's own commonest colour then answered 24% in a scene for the effect,
    /// for the scene without it, and for anything else: most of a level's frame
    /// is level.
    #[test]
    fn coverage_is_what_the_effect_added_to_the_view() {
        let flat: Vec<u8> = [90u8, 90, 100, 255].repeat(64);

        // Nothing added is nothing, whatever the view happens to look like.
        assert_eq!(coverage_against(&flat, &flat), 0.0);
        // …including a busy view: this is the scene case, where every earlier
        // version of this answered "a quarter of the frame" for an effect that
        // was not there.
        let mut busy = flat.clone();
        for (i, p) in busy.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            p[0] = (i * 4) as u8;
        }
        assert_eq!(coverage_against(&busy, &busy), 0.0);

        // A quarter of it changed is a quarter — including when the changed part
        // is the corner. Referring to the frame's own background made an effect
        // that reached one the reference for its own frame, and reported the
        // other three quarters as the anomaly.
        let mut lit = busy.clone();
        for p in lit.as_chunks_mut::<4>().0.iter_mut().take(16) {
            p[1] = 255;
        }
        assert!((coverage_against(&lit, &busy) - 0.25).abs() < 1e-6);

        // A level or two of difference is dither, not an effect. Counting it
        // would report every empty frame as faintly occupied.
        let mut noise = busy.clone();
        for (i, p) in noise.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            p[2] = p[2].wrapping_add((i % 3) as u8);
        }
        assert_eq!(coverage_against(&noise, &busy), 0.0);

        // Nothing at all, and a mismatched pair, are both nothing rather than a
        // panic or a division by zero.
        assert_eq!(coverage_against(&[], &[]), 0.0);
        assert_eq!(coverage_against(&flat, &flat[..8]), 0.0);
    }

    /// **A single big billboard is not a point.** Framing on particle centres
    /// alone puts the camera inside a one-particle effect — its cloud is one
    /// position, so the box has no size, so the fit distance collapses.
    #[test]
    fn framing_accounts_for_how_big_the_particles_are() {
        let (centre, half) = bounds_of(&[(Vec3::ZERO, 8.0)]).expect("one particle is something");
        assert_eq!(centre, Vec3::ZERO);
        assert!(half.min_element() >= 4.0, "an 8-unit billboard needs 4 units of room: {half}");
        // Nothing at all has no framing, and says so rather than inventing one.
        assert!(bounds_of(&[]).is_none());
    }

    /// **The camera has to be a rotation.** `Mat3::from_cols` will happily build
    /// a determinant −1 matrix out of a basis whose cross products went the
    /// wrong way round, and `Quat::from_mat3` of one is not a rotation — it
    /// renders, it exits 0, and the effect is somewhere else in the frame with
    /// nothing saying why. That shipped once already.
    #[test]
    fn the_fitted_camera_is_a_right_handed_rotation() {
        let cam = fitted_camera(&[(Vec3::ZERO, 1.0)], DVec3::ZERO, 16.0 / 9.0);
        let m = floptle_core::math::Mat3::from_quat(cam.rotation);
        assert!((m.determinant() - 1.0).abs() < 1e-4, "not a rotation: det {}", m.determinant());
        // And it looks at what it was fitted to, rather than merely near it.
        let fwd = (cam.rotation * Vec3::NEG_Z).normalize();
        let to_target = (-cam.world_position).as_vec3().normalize();
        assert!(fwd.dot(to_target) > 0.999, "it is not pointing at the effect: {fwd} vs {to_target}");
        // …the right way up. Flipping the basis the OTHER way keeps the
        // determinant at +1 and rolls the picture 180°: the effect is all there,
        // upside down, and nothing about a symmetrical burst would say so.
        let cam_up = cam.rotation * Vec3::Y;
        assert!(cam_up.dot(Vec3::Y) > 0.0, "the camera is upside down: up is {cam_up}");
    }

    /// **A tight fit is the whole value of the picture.** Fitting the sphere
    /// around the box instead of the box is √3 too far for a cube, and the
    /// effect ends up a third of the frame it was framed for.
    #[test]
    fn a_cube_of_particles_fills_most_of_the_frame() {
        let cloud: Vec<(Vec3, f32)> = [-1.0f32, 1.0]
            .iter()
            .flat_map(|&x| [-1.0f32, 1.0].iter().map(move |&y| (x, y)))
            .flat_map(|(x, y)| [-1.0f32, 1.0].iter().map(move |&z| (Vec3::new(x, y, z), 0.0)))
            .collect();
        let cam = fitted_camera(&cloud, DVec3::ZERO, 1.0);
        // Half the vertical view at the box centre, against the box's own reach.
        let dist = cam.world_position.length() as f32;
        let half_view = (0.9f32 * 0.5).tan() * dist;
        assert!(
            half_view < Vec3::splat(1.0).length() * 1.35,
            "the frame is {half_view:.2} across a box that reaches {:.2} — fitted to the \
             sphere rather than the box",
            Vec3::splat(1.0).length()
        );
    }
}
