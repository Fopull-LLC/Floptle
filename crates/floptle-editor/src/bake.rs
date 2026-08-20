//! `floptle bake gi` — **bake a scene's light probes, and exit.**
//!
//! Its help has always said "and exit". It did not: the flag set a marker, the
//! editor opened a window, and a frame hook noticed the marker, ran the bake a
//! slice at a time and asked the window to close when it finished. On a build
//! server that is not a slow bake, it is no bake at all — there is no display to
//! open, so the process dies before the first slice.
//!
//! Nothing about the bake needed the window. It renders the scene through the
//! same offscreen path `shot` uses, it writes its result to disk itself, and it
//! is already sliced into frame-sized pieces so the editor stays responsive. All
//! the window contributed was a clock to hang the slices on, and a loop here is
//! a better one: it runs the slices back to back instead of one per refresh.
//!
//! The editor keeps its own path exactly as it was — a bake you can watch, that
//! you can cancel, with a progress bar. This is the same bake with nobody
//! watching.

use std::path::Path;

use floptle_render::Gpu;

/// Run the verb. Returns the process exit code.
pub(crate) fn run(root: &Path, scene: Option<&str>, json: bool) -> i32 {
    if !root.join("project.ron").is_file() {
        eprintln!("{} is not a project directory (no project.ron)", root.display());
        return 2;
    }

    // A GPU first, then the project — the same order `shot` uses and for the
    // same reason: `open_project` imports models and adopts painted data, and
    // without a device it quietly does neither.
    //
    // The size is the offscreen scratch the device is configured with, not the
    // bake's own: every probe face renders into a texture the bake allocates
    // from the Light Probes node's quality setting.
    let gpu = Gpu::headless_hdr(64, 64);
    gpu.device.on_uncaptured_error(std::sync::Arc::new(|e: wgpu::Error| {
        eprintln!("this machine's graphics driver could not build the renderer, so there is \
                   nothing to bake with:\n  {e}");
        std::process::exit(1);
    }));
    let mut ed = crate::Editor {
        show_gizmos: false,
        // The bake reports through the Console — its progress line, the "no
        // Light Probes node" refusal, and any GPU error. Under `--json` that is
        // collected into the document; otherwise it belongs on stderr, because
        // stdout carries the answer.
        console: crate::console::ConsoleState { mirror_to_stderr: !json, ..Default::default() },
        ..Default::default()
    };
    ed.attach_gpu(gpu);
    ed.open_project(root.to_path_buf());
    if let Some(s) = scene {
        let Some(path) = crate::inspect::resolve_scene(root, s) else {
            eprintln!("no scene called {s} under {}", root.join("scenes").display());
            return 1;
        };
        ed.open_scene_file(&path.to_string_lossy());
    }
    let opened: Vec<crate::console::ConsoleEntry> = std::mem::take(&mut ed.console.entries);

    if !ed.start_gi_bake() {
        eprintln!(
            "{} has no enabled Light Probes node, so there is nothing to bake — add one, or \
             name another scene with --scene",
            ed.scene_rel_or_default()
        );
        return 1;
    }

    // **Slice until it is done.** `step_gi_bake` renders as many probes as fit
    // in its frame budget and hands the rest back; it clears `gi_bake` when the
    // last bounce has been written. The budget is pointless here — nobody is
    // waiting on a refresh — but it costs one extra call per slice and keeping
    // the editor's stepping function unchanged is worth far more than saving
    // them.
    let mut slices = 0u32;
    while ed.gi_bake.is_some() {
        ed.step_gi_bake();
        slices += 1;
        // A bake that is not progressing would otherwise spin here forever. The
        // step either advances or finishes, so this is a backstop against a
        // future change rather than a case anybody has seen — and a loop that
        // cannot end is worse than a wrong answer.
        if slices > 1_000_000 {
            eprintln!("the bake stopped making progress after {slices} passes");
            return 1;
        }
    }

    report(&opened, &ed.console, json)
}

fn report(
    opened: &[crate::console::ConsoleEntry],
    console: &crate::console::ConsoleState,
    json: bool,
) -> i32 {
    use floptle_script::LogLevel;
    let all = || opened.iter().chain(console.entries.iter());
    let errors = all().filter(|e| e.level == LogLevel::Error).count();

    if json {
        let doc = serde_json::json!({
            "ok": errors == 0,
            "errors": errors,
            "log": all()
                .map(|e| serde_json::json!({
                    "level": match e.level {
                        LogLevel::Error => "error",
                        LogLevel::Warn => "warning",
                        LogLevel::Debug => "print",
                    },
                    "message": e.msg,
                    "count": e.count,
                }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return i32::from(errors > 0);
    }
    // The bake's own summary line is the answer, so it goes to stdout — it says
    // how many probes, how many bounces, how long, and what it wrote.
    for e in all().filter(|e| e.level == LogLevel::Debug && e.msg.starts_with("baked GI")) {
        println!("{}", e.msg);
    }
    i32::from(errors > 0)
}
