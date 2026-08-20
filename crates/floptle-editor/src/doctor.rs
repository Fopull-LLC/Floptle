//! `floptle doctor` — **can this machine do the things the commands need?**
//!
//! `floptle help --json` says which verbs need a display. It cannot say whether
//! the machine reading it has one, and "needs a GPU" turns out to be two
//! questions rather than one: is there an adapter, and can the engine's shaders
//! actually be built on it. A release was tagged and burned on the gap between
//! those — there was an adapter, it was OpenGL, and the renderer cannot be
//! built there.
//!
//! So this does not ask the adapter what it can do. It builds the renderer and
//! reports what happened, which is the only answer worth having.

/// What a machine can do, as one report.
struct Findings {
    engine: String,
    adapter: Option<AdapterFacts>,
    /// `Ok(())` when a real renderer was created on it.
    renders: Result<(), String>,
}

struct AdapterFacts {
    name: String,
    backend: String,
    driver: String,
    kind: String,
}

/// Run the verb. Returns the process exit code.
pub(crate) fn run(json: bool) -> i32 {
    let f = examine();
    if json {
        let doc = serde_json::json!({
            "engine": f.engine,
            "adapter": f.adapter.as_ref().map(|a| serde_json::json!({
                "name": a.name,
                "backend": a.backend,
                "driver": a.driver,
                "kind": a.kind,
            })),
            // The question a caller actually has: can it run `shot`, `bake gi`,
            // `open` and `play`. Everything else needs no adapter at all.
            "canRender": f.renders.is_ok(),
            "whyNot": f.renders.as_ref().err(),
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return i32::from(f.renders.is_err());
    }

    println!("floptle {}", f.engine);
    match &f.adapter {
        Some(a) => {
            println!("  graphics    {} ({})", a.name, a.kind);
            println!("  backend     {}", a.backend);
            if !a.driver.is_empty() {
                println!("  driver      {}", a.driver);
            }
        }
        // No facts means the probe never got far enough to ask for them, and
        // the reason is on the line below. Saying "none" here would contradict
        // it when the adapter exists and the device is what failed.
        None => println!("  graphics    could not be inspected"),
    }
    match &f.renders {
        Ok(()) => println!("  rendering   yes — shot, bake gi, open and play will run here"),
        Err(why) => {
            println!("  rendering   NO — {why}");
            println!();
            // Read out of the verb table rather than typed here, so this list
            // cannot come to name a command that does not exist or miss one
            // that does — the same reason `--help` is generated from it.
            let fine: Vec<&str> =
                crate::cli::VERBS.iter().filter(|v| !v.needs_gpu).map(|v| v.name).collect();
            println!("Everything that needs no display still works: {}.", fine.join(", "));
            println!("`floptle help --json` marks which is which with `needsGpu`.");
        }
    }
    i32::from(f.renders.is_err())
}

fn examine() -> Findings {
    let engine = env!("CARGO_PKG_VERSION").to_string();

    // **Ask by doing.** A device can be created on an adapter whose shaders the
    // engine cannot compile, so nothing short of building the renderer answers
    // this — see the OpenGL note in HANDOFF.
    //
    // A validation failure reaches the uncaptured-error handler rather than
    // this thread, so the handler records it and the check reads what it left.
    // `Gpu::headless_*` panics when there is no adapter at all — a fine answer
    // to give a person and a poor one to give a caller who asked a yes/no
    // question. The hook goes with it: this binary writes a crash report on
    // panic, and a machine truthfully reporting that it has no graphics card
    // must not also file a bug about it.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let probed = std::panic::catch_unwind(build_a_renderer);
    std::panic::set_hook(hook);
    let (adapter, renders) = match probed {
        Ok(r) => r,
        // The panic payload is an `expect` message, and it distinguishes the two
        // ways this fails: no adapter at all, versus an adapter that would not
        // give out a device. A caller staring at a machine with a graphics card
        // in it should not be told there is none.
        Err(payload) => {
            let said = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("");
            let why = if said.contains("device") {
                "there is a graphics adapter, but it would not give floptle a device to render with"
            } else {
                "this machine has no graphics adapter floptle can use"
            };
            (None, Err(why.into()))
        }
    };
    Findings { engine, adapter, renders }
}

/// Create a device and build the raster pipeline on it.
fn build_a_renderer() -> (Option<AdapterFacts>, Result<(), String>) {
    let gpu = floptle_render::Gpu::headless_hdr(64, 64);
    let info = gpu.adapter.get_info();
    let facts = AdapterFacts {
        name: info.name.clone(),
        backend: format!("{:?}", info.backend),
        driver: if info.driver_info.is_empty() {
            info.driver.clone()
        } else {
            format!("{} {}", info.driver, info.driver_info)
        },
        kind: format!("{:?}", info.device_type).to_lowercase(),
    };

    // Collect what the device refuses instead of letting it panic the process.
    let failed = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = failed.clone();
    gpu.device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
        if let Ok(mut s) = sink.lock()
            && s.is_empty()
        {
            *s = e.to_string();
        }
    }));

    // The raster pipeline is the one that could not be built on OpenGL, so it
    // is the one worth building.
    let _raster = floptle_render::Raster::new(&gpu);
    let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    let why = failed.lock().map(|s| s.clone()).unwrap_or_default();
    let renders = if why.is_empty() {
        Ok(())
    } else {
        // One line: the whole validation message is a paragraph of backend
        // detail, and the first line is the part that says what happened.
        Err(why.lines().rfind(|l| !l.trim().is_empty()).unwrap_or(&why).trim().to_string())
    };
    (Some(facts), renders)
}

