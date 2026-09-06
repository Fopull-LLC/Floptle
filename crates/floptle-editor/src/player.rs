//! **The standalone player: a shipped game, with no editor in it.**
//!
//! This is the binary an export ships (`floptle-player`, renamed to the game's
//! own title). It is a window, an input pump and [`Editor::player_frame`] —
//! nothing else. There is no dock, no Inspector, no gizmos, no asset browser
//! and, when built without the crate's `editor-ui` feature, no egui compiled in
//! at all.
//!
//! ## Why it lives in the editor's crate
//!
//! Because the engine does. The World, the script host, the physics sim, the
//! renderer and Play mode all hang off [`Editor`], and the two functions that
//! *are* the game — [`Editor::play_step`] and [`Editor::render_game_into`] —
//! are shared verbatim with the authoring application. A player that
//! reimplemented either would drift from the one the editor shows you, and the
//! difference would only ever be found by somebody playing the shipped build.
//!
//! So the split is by *feature*, not by copy: `floptle-player` is a three-line
//! binary over `run_player`, and the editor half of this crate is compiled out
//! from under it.
//!
//! ## What a player does that the editor does not
//!
//! It owns the window outright. Input is never somebody else's — there is no
//! panel to have focus, no Scene view to fly a camera around, and no Escape
//! that means "back out of a tool". A script asking for the mouse
//! (`input.lockMouse()`) simply gets it.

use std::path::PathBuf;
use std::sync::Arc;
use floptle_core::time::Instant;

use floptle_core::math::Vec2;
use floptle_render::Gpu;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::Editor;

/// Run a shipped game and return when its window closes.
///
/// The project comes from a `floptle-game.ron` manifest beside the binary —
/// what **File ⏵ Export Game…** writes. A path argument overrides it, which is
/// how the player is run against a project during development
/// (`floptle-player path/to/project`).
#[cfg(not(target_arch = "wasm32"))]
pub fn run_player() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "{} player v{}\n\n\
             Usage: {} [PROJECT]\n\n\
             With no argument, plays the game described by the floptle-game.ron\n\
             manifest beside this binary — which is what an exported build is.\n\
             A PROJECT path plays that project directly, for development.",
            floptle_core::ENGINE_NAME,
            crate::distribution_version(),
            args.first().map(String::as_str).unwrap_or("floptle-player"),
        );
        return;
    }
    let explicit = args.iter().skip(1).find(|a| !a.starts_with('-')).map(PathBuf::from);
    // `--shot <png> [--frames N]`: play for N frames, photograph the frame that
    // was actually presented, and exit. The way a build gets verified — by CI,
    // and by anyone asking "does the export still draw the game" without
    // needing a screenshot tool or a person at the window.
    let flag = |name: &str| {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).filter(|v| !v.starts_with('-'))
    };
    let shot = flag("--shot").map(PathBuf::from);
    let shot_at: u32 = flag("--frames").and_then(|v| v.parse().ok()).unwrap_or(60);

    // An exported build: the manifest names the title, the assets folder beside
    // it, and the Steam App ID (`project.ron` is not shipped, so the manifest is
    // the only place a build can read that from).
    let manifest = crate::export::load_game_manifest();
    let (title, project, steam_settings, shipped) = match (explicit, manifest) {
        (Some(p), _) => {
            let title = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "game".into());
            let steam = floptle_scene::load_project(&p.join("project.ron")).steam;
            (title, p, steam, false)
        }
        (None, Some((m, dir))) => {
            let project = dir.join(&m.project);
            (m.title, project, m.steam, true)
        }
        (None, None) => {
            eprintln!(
                "no game to play: there is no floptle-game.ron beside this binary, and no \
                 project path was given.\n\
                 An exported build ships both; to play a project directly, pass its folder."
            );
            std::process::exit(2);
        }
    };
    if !floptle_vfs::is_file(project.join("project.ron")) {
        eprintln!(
            "{} is not a project folder (no project.ron) — this build's assets are missing \
             or were moved away from the binary",
            project.display()
        );
        std::process::exit(1);
    }

    // Steam's lifecycle activates before any window or GPU exists, so
    // `RestartAppIfNecessary` can still exit the process.
    let steam_platform = match crate::steam_boot::resolve_app_id(steam_settings, false) {
        Some(app_id) => crate::steam_boot::boot(app_id, shipped),
        None => None,
    };

    println!("{title} — {} v{}", floptle_core::ENGINE_NAME, crate::distribution_version());

    let event_loop = crate::build_event_loop(steam_platform.is_some());
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = Player::new(title, project);
    app.shot = shot;
    app.shot_at = shot_at;
    // The platform capability boundary (achievements, overlay, rich presence)
    // is handed to the script host, which is what `steam.*` answers from.
    if let Some(platform) = steam_platform {
        app.ed.script_host.set_platform(platform);
    }
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("the game's window loop ended with an error: {e}");
    }
    if app.exit_code != 0 {
        std::process::exit(app.exit_code);
    }
}

/// The window + input half. Everything else is [`Editor`].
struct Player {
    ed: Editor,
    title: String,
    project: PathBuf,
    fullscreen: bool,
    /// The active grab is a CONFINE rather than a lock (X11 has no OS-level
    /// lock), so the cursor is re-centred every frame.
    grabbed_soft: bool,
    /// The lock state the window was last actually asked for.
    ///
    /// Asking is not free on the web — each ask is a `requestPointerLock`, and
    /// one made too soon after the player pressed Escape is refused and logged
    /// — so the ask is made when the answer changes rather than every frame.
    grab_asked: bool,
    /// Web only: has the browser been seen to actually grant the lock?
    ///
    /// The request is asynchronous, so "not locked" in the frame after asking
    /// means "no answer yet", not "refused". Once the lock has been observed,
    /// losing it means the player took it back.
    #[cfg(target_arch = "wasm32")]
    web_lock_held: bool,
    /// `--shot`: where to write the photographed frame, and which frame.
    shot: Option<PathBuf>,
    shot_at: u32,
    frames: u32,
    exit_code: i32,
    /// The page's canvas, handed over at start and consumed by `resumed`.
    #[cfg(target_arch = "wasm32")]
    canvas: Option<web_sys::HtmlCanvasElement>,
    /// A device requested in a browser arrives asynchronously; `resumed`
    /// leaves it here and the first redraw that finds it finishes booting.
    #[cfg(target_arch = "wasm32")]
    pending_gpu: std::rc::Rc<std::cell::RefCell<Option<Gpu>>>,
    /// A frame copied into a mappable buffer, waiting for the map to be safe
    /// to ask for — see [`Player::capture_web`].
    #[cfg(target_arch = "wasm32")]
    pending_shot: Option<web::PendingShot>,
}

impl Player {
    /// A player for `project`, titled `title`, not yet running.
    fn new(title: String, project: PathBuf) -> Self {
        Self {
            // `player_mode` is what every "is this a build?" test in the engine
            // already asks, and it is permanently true here — see the editor-chrome
            // rule in `docs/`: `if playing` is fixed furniture on a game.
            ed: Editor {
                player_mode: true,
                show_gizmos: false,
                game_title: title.clone(),
                // A build has no Console panel, so the game's own prints go to
                // stderr and a shipped build stays debuggable from a terminal.
                console: crate::console::ConsoleState { mirror_to_stderr: true, ..Default::default() },
                ..Default::default()
            },
            title,
            project,
            fullscreen: false,
            grabbed_soft: false,
            grab_asked: false,
            #[cfg(target_arch = "wasm32")]
            web_lock_held: false,
            shot: None,
            shot_at: 60,
            frames: 0,
            exit_code: 0,
            #[cfg(target_arch = "wasm32")]
            canvas: None,
            #[cfg(target_arch = "wasm32")]
            pending_gpu: Default::default(),
            #[cfg(target_arch = "wasm32")]
            pending_shot: None,
        }
    }

    /// The GPU is attached, the project opened, and the game started — the
    /// three things that turn a window into the game, in the order that
    /// works: `open_project` imports models and adopts paint, and both need a
    /// device. The same order `floptle shot` uses, and the reason it is the
    /// tested one.
    fn boot(&mut self, gpu: Gpu) {
        self.ed.attach_gpu(gpu);
        self.ed.open_project(self.project.clone());
        let now = Instant::now();
        self.ed.started = Some(now);
        self.ed.last = Some(now);
        // Straight into the game. There is no Play button in a build.
        self.ed.toggle_play();
    }
}

impl ApplicationHandler for Player {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.ed.window.is_some() {
            return;
        }
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes().with_title(&self.title);
        #[cfg(not(target_arch = "wasm32"))]
        {
            attrs = attrs.with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        }
        #[cfg(target_arch = "wasm32")]
        {
            // The page's own canvas, at the size the page gave it.
            use winit::platform::web::WindowAttributesExtWebSys;
            attrs = attrs.with_canvas(self.canvas.take()).with_prevent_default(true);
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("could not open a window: {e}");
                event_loop.exit();
                return;
            }
        };
        self.ed.window = Some(window.clone());
        #[cfg(not(target_arch = "wasm32"))]
        {
            // **The GPU before the project** — see `boot`.
            self.boot(Gpu::new(window.clone()));
        }
        #[cfg(target_arch = "wasm32")]
        {
            // A browser answers a device request asynchronously. The window
            // keeps asking to be redrawn until the device lands, and the
            // redraw that finds it boots the game.
            let slot = self.pending_gpu.clone();
            let w = window.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let gpu = Gpu::new_async(w).await;
                *slot.borrow_mut() = Some(gpu);
            });
        }
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            // A build's window close IS quitting: there is no unsaved work to
            // ask about.
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.ed.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.ed.cursor = Some(Vec2::new(position.x as f32, position.y as f32));
            }
            // Alt-tabbing away is the other way people ask for the cursor
            // back, and the one they find on their own: the compositor drops
            // the grab on focus loss whether the app agrees or not. Without
            // this the frame loop simply took it again — reaching out of an
            // unfocused window to steal the pointer off whatever the player
            // had switched to.
            WindowEvent::Focused(false) => {
                self.ed.set_cursor_freed(true);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                let i = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                    MouseButton::Other(_) => return,
                };
                // A click is how the pointer goes back to the game after
                // Escape — and on the web it is the only thing that can, since
                // `requestPointerLock` needs a gesture to hang off.
                if pressed && i == 0 {
                    self.ed.set_cursor_freed(false);
                }
                self.ed.track_mouse_button(i, pressed);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.ed.input_scroll += match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let pressed = event.state == ElementState::Pressed;
                self.feed_key(code, pressed, event.text.as_deref());
            }
            WindowEvent::RedrawRequested => {
                self.frame(event_loop);
                if let Some(w) = self.ed.window.as_ref() {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _e: &ActiveEventLoop, _d: DeviceId, event: DeviceEvent) {
        // Raw motion, so a look never stalls against a window edge while the
        // pointer is grabbed.
        if let DeviceEvent::MouseMotion { delta } = event {
            self.ed.input_mouse_delta.0 += delta.0 as f32;
            self.ed.input_mouse_delta.1 += delta.1 as f32;
        }
    }
}

impl Player {
    /// One frame, plus the two things only a window can do: honour the game's
    /// request for the pointer, and re-centre a confine-only grab.
    fn frame(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(target_arch = "wasm32")]
        if self.ed.gpu.is_none() {
            let Some(gpu) = self.pending_gpu.borrow_mut().take() else { return };
            let mut gpu = gpu;
            // The canvas reached its CSS size while the device request was in
            // flight, and the `Resized` that announced it found no device to
            // apply to — so ask the window now, or the surface stays at the
            // 1x1 it was configured with.
            if let Some(w) = self.ed.window.as_ref() {
                let size = w.inner_size();
                if (size.width, size.height) != (gpu.config.width, gpu.config.height) {
                    gpu.resize(size.width, size.height);
                }
            }
            self.boot(gpu);
            web::log(&format!(
                "booted: {} at {}x{}, {} nodes",
                self.title,
                self.ed.gpu.as_ref().map_or(0, |g| g.config.width),
                self.ed.gpu.as_ref().map_or(0, |g| g.config.height),
                self.ed.world.len()
            ));
        }
        self.frames += 1;
        #[cfg(target_arch = "wasm32")]
        if self.frames.is_multiple_of(30) {
            web::log(&format!("frame {} · {:.1} ms", self.frames, self.ed.frame_ms));
        }
        // A browser cannot block on a GPU readback, so the page's capture is
        // its own path (`capture_web`) and the frame itself is never asked to.
        let capture = !cfg!(target_arch = "wasm32") && self.shot.is_some() && self.frames >= self.shot_at;
        let shot = self.ed.player_frame(capture);
        #[cfg(target_arch = "wasm32")]
        {
            if self.shot.is_some() && self.frames == self.shot_at {
                self.capture_web();
            }
            // The map is asked for a few frames after the copy — never in the
            // same task as it. See `capture_web`.
            if self.pending_shot.as_ref().is_some_and(|p| self.frames >= p.due)
                && let Some(pending) = self.pending_shot.take()
            {
                web::read_shot(pending);
            }
        }
        if capture {
            self.write_shot(shot);
            event_loop.exit();
            return;
        }
        let Some(window) = self.ed.window.clone() else { return };
        // **The browser's own Escape.** A page cannot see the keypress that
        // exits pointer lock — the browser consumes it deliberately, so a
        // page cannot trap the player inside a locked cursor — which means the
        // Escape handling in `feed_key` never runs on the web. Ask the
        // document instead, and treat a lock that was granted and is now gone
        // as the player asking for their pointer back.
        #[cfg(target_arch = "wasm32")]
        {
            let locked = web::pointer_locked();
            if locked {
                self.web_lock_held = true;
            } else if self.web_lock_held {
                self.web_lock_held = false;
                self.ed.set_cursor_freed(true);
            }
        }
        // What the GAME wants, minus the player having taken the pointer back
        // with Escape. `script_mouse_lock` alone is the game's standing wish,
        // and a first-person camera renews it every frame from `update`: read
        // straight, it put the grab back on the frame after Escape and the key
        // looked like it did nothing at all. A build promised that key in
        // `docs/export-builds.md` and did not have it.
        let want = self.ed.game_holds_cursor();
        // Native: re-assert while the game wants the pointer, because a
        // compositor drops a grab on focus loss without telling the app and
        // asking again is the only way back. On the web the same loop is a
        // `requestPointerLock` per frame, every one of them refused and logged
        // for a full second after an Escape.
        let reassert = !cfg!(target_arch = "wasm32");
        if want != self.grab_asked || (want && reassert) {
            self.grab_asked = want;
            self.grabbed_soft = crate::grab_cursor(&window, want);
            self.ed.cursor_lock_soft = self.grabbed_soft;
        }
        if want && self.grabbed_soft {
            let size = window.inner_size();
            let centre = winit::dpi::PhysicalPosition::new(size.width / 2, size.height / 2);
            let _ = window.set_cursor_position(centre);
        }
    }

    /// Feed one key event to the script `input` API, the action layer and any
    /// focused game-UI text field — the three views of the keyboard, filled
    /// together so they can never disagree within a frame.
    ///
    /// The editor's version of this is threaded with questions a build cannot
    /// ask: is a panel typing, is the pointer over the Scene view, is a keybind
    /// being re-recorded. None of them exist here, which is most of why this is
    /// short.
    fn feed_key(&mut self, code: KeyCode, pressed: bool, text: Option<&str>) {
        // **Escape frees a script-locked cursor.** The one gesture a build must
        // answer itself: a game that calls `setMouseLocked(true)` and then
        // shows a menu — or crashes, or simply has a bug — has otherwise taken
        // the player's pointer with no way to get it back short of killing the
        // window. The editor has had this since the lock existed; the player
        // binary shipped without it, while `docs/export-builds.md` promised it.
        //
        // The game still SEES the key: plenty of games open their pause menu on
        // it, and swallowing it would break them. This only releases the grab.
        if pressed && code == KeyCode::Escape {
            self.ed.set_cursor_freed(true);
        }
        // The two window chords a build answers itself.
        if pressed
            && (code == KeyCode::F11
                || (code == KeyCode::Enter
                    && self.ed.input_keys.contains("lalt")
                    | self.ed.input_keys.contains("ralt")))
        {
            self.toggle_fullscreen();
        }
        if let Some(name) = crate::key_name(code) {
            if pressed {
                if self.ed.input_keys.insert(name.to_string()) {
                    self.ed.input_keys_pressed.insert(name.to_string());
                    self.ed.tick_keys_pressed.insert(name.to_string());
                }
            } else if self.ed.input_keys.remove(name) {
                self.ed.input_keys_released.insert(name.to_string());
                self.ed.tick_keys_released.insert(name.to_string());
            }
        }
        // What the player TYPED, as opposed to which key they hit: layout
        // resolved by the OS, control characters left as actions.
        if pressed
            && let Some(text) = text
        {
            let typed: String = text.chars().filter(|c| !c.is_control()).collect();
            self.ed.input_typed.push_str(&typed);
            self.ed.tick_typed.push_str(&typed);
        }
        if pressed {
            self.ed.note_ui_text_key(code);
        }
        self.ed.note_action_key(code, pressed);
    }

    /// Write the photographed frame, or say why there is none. A `--shot` that
    /// silently produced nothing would read as a build that rendered fine.
    fn write_shot(&mut self, shot: Option<(Vec<u8>, u32, u32)>) {
        let Some(path) = self.shot.clone() else { return };
        let Some((px, w, h)) = shot else {
            eprintln!(
                "--shot: this device's surface cannot be copied from, so there is no frame \
                 to write (the game itself is unaffected)"
            );
            self.exit_code = 1;
            return;
        };
        let Some(buf) = image::RgbaImage::from_raw(w, h, px) else {
            eprintln!("--shot: the frame came back the wrong size");
            self.exit_code = 1;
            return;
        };
        match buf.save(&path) {
            Ok(()) => println!("wrote {} ({w}x{h}, frame {})", path.display(), self.frames),
            Err(e) => {
                eprintln!("--shot: could not write {}: {e}", path.display());
                self.exit_code = 1;
            }
        }
    }

    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        if let Some(w) = self.ed.window.as_ref() {
            w.set_fullscreen(self.fullscreen.then_some(Fullscreen::Borderless(None)));
        }
    }
}

/// The browser: the same [`Player`], started from a page.
#[cfg(target_arch = "wasm32")]
pub mod web {
    use std::path::PathBuf;

    use super::Player;
    use wasm_bindgen::prelude::*;
    use winit::event_loop::EventLoop;
    use winit::platform::web::EventLoopExtWebSys;

    #[wasm_bindgen]
    extern "C" {
        /// `window.floptleShotAt()`: the frame the page wants photographed, or 0.
        /// Optional — a page without the harness hooks does not define it.
        #[wasm_bindgen(js_namespace = window, js_name = floptleShotAt, catch)]
        fn page_shot_at() -> Result<u32, JsValue>;
        /// `window.floptleCapture(rgba, w, h)`: the photographed frame.
        #[wasm_bindgen(js_namespace = window, js_name = floptleCapture, catch)]
        fn page_capture(rgba: &[u8], width: u32, height: u32) -> Result<(), JsValue>;
        /// `window.floptleLog(line)`: the page's transcript.
        #[wasm_bindgen(js_namespace = window, js_name = floptleLog, catch)]
        fn page_log(line: &str) -> Result<(), JsValue>;
    }

    /// Does the document hold a pointer lock right now?
    ///
    /// The page's own view of the grab, which is the only reliable one on the
    /// web: the player exits pointer lock with Escape and the browser does not
    /// deliver that keypress, so nothing else in the game ever learns it
    /// happened.
    pub(super) fn pointer_locked() -> bool {
        web_sys::window()
            .and_then(|w| w.document())
            .is_some_and(|d| d.pointer_lock_element().is_some())
    }

    /// A line to the page's transcript — what `eprintln!` cannot do here:
    /// Rust's stderr is a no-op on `wasm32-unknown-unknown`. `false` when the
    /// page defines no `floptleLog` (a host page of someone's own).
    pub(crate) fn log(line: &str) -> bool {
        page_log(line).is_ok()
    }

    /// A frame copied into a mappable buffer, waiting for `due` to arrive.
    pub(super) struct PendingShot {
        pub(super) buf: wgpu::Buffer,
        pub(super) w: u32,
        pub(super) h: u32,
        pub(super) padded: u32,
        pub(super) bgra: bool,
        /// The frame number at which the map may be asked for.
        pub(super) due: u32,
    }

    /// Map the copied frame and hand its pixels to the page. The callback
    /// keeps the buffer alive until the map completes.
    pub(super) fn read_shot(shot: PendingShot) {
        let PendingShot { buf, w, h, padded, bgra, .. } = shot;
        let bpp = 4u32;
        let keep = buf.clone();
        buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            if let Err(e) = r {
                // Say WHY. A capture that fails silently reads as a build that
                // never drew. (wgpu's own sentence is fixed; the page's
                // `mapAsync` hook keeps the browser's reason alongside it.)
                log(&format!("capture: the frame could not be mapped: {e}"));
                let _ = page_capture(&[], 0, 0);
                return;
            }
            let data = keep.slice(..).get_mapped_range();
            let mut rgba = Vec::with_capacity((w * h * bpp) as usize);
            for y in 0..h {
                let row = (y * padded) as usize;
                for x in 0..w {
                    let i = row + (x * bpp) as usize;
                    if bgra {
                        rgba.extend_from_slice(&[data[i + 2], data[i + 1], data[i], data[i + 3]]);
                    } else {
                        rgba.extend_from_slice(&data[i..i + 4]);
                    }
                }
            }
            drop(data);
            keep.unmap();
            let _ = page_capture(&rgba, w, h);
        });
    }

    /// Start the game whose bundle is mounted (`floptle_vfs::mount`) on
    /// `canvas`, and hand the event loop to the browser. Returns rather than
    /// blocking; the game runs from the page's animation frames.
    ///
    /// The error is one sentence for the page to show: a bundle with no
    /// manifest, or a manifest naming a project that is not in it.
    pub fn start(canvas: web_sys::HtmlCanvasElement) -> Result<(), String> {
        let (manifest, dir) = crate::export::load_game_manifest()
            .ok_or("the game bundle has no floptle-game.ron at its root — it was not made by File ⏵ Export Game…")?;
        let project = dir.join(&manifest.project);
        if !floptle_vfs::is_file(project.join("project.ron")) {
            return Err(format!(
                "the bundle's manifest names {} as the project, and it holds no project.ron",
                project.display()
            ));
        }
        floptle_vfs::open_saves(&manifest.title)?;
        let event_loop = EventLoop::new().map_err(|e| format!("no event loop: {e}"))?;
        let mut app = Player::new(manifest.title, project);
        app.canvas = Some(canvas);
        // The harness's frame to photograph, if the page names one.
        let shot_at = page_shot_at().unwrap_or(0);
        if shot_at > 0 {
            app.shot = Some(PathBuf::new());
            app.shot_at = shot_at;
        }
        event_loop.spawn_app(app);
        Ok(())
    }

    impl Player {
        /// The browser's `--shot`: render this frame again into a texture the
        /// engine owns and copy it into a mappable buffer. The map itself is
        /// asked for a few frames later ([`read_shot`], driven from `frame`) —
        /// a map requested in the same task as the copy is legal by the spec
        /// and is rejected in practice, which is the same pattern the bring-up
        /// probe settled on.
        ///
        /// A second render of the same frame rather than a read of the one
        /// presented: a browser's canvas cannot be read back, and its surface
        /// offers neither COPY_SRC nor TEXTURE_BINDING, so there is nothing to
        /// copy FROM. Named here because the desktop's `--shot` does read the
        /// presented image and the two are therefore not the same guarantee.
        pub(super) fn capture_web(&mut self) {
            let Some(gpu) = self.ed.gpu.as_ref() else { return };
            let (w, h) = (gpu.config.width, gpu.config.height);
            let format = gpu.surface_format();
            let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("player-shot"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                // TEXTURE_BINDING as well as the obvious two: the game's UI can
                // SAMPLE what has been drawn so far (the capture effect behind a
                // frosted panel), and a target that cannot be sampled makes every
                // such bind group invalid — a wall of errors, and no picture.
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let (depth_view, depth_tex) = (gpu.depth_view().clone(), gpu.depth_texture().clone());
            let elapsed = self.ed.started.map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0);
            // This target is ours and carries TEXTURE_BINDING, so the UI's
            // backdrop capture works here even where the swapchain's does not.
            self.ed.render_game_into(view, depth_view, Some(depth_tex), [0.0, 0.0], [w as f32, h as f32], elapsed, true, true);
            let Some(gpu) = self.ed.gpu.as_ref() else { return };
            let bpp = 4u32;
            let padded = (w * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("player-shot-readback"),
                size: (padded * h) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("player-shot") });
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buf,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) },
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            gpu.queue.submit([enc.finish()]);
            let bgra = format.remove_srgb_suffix() == wgpu::TextureFormat::Bgra8Unorm;
            self.pending_shot = Some(PendingShot { buf, w, h, padded, bgra, due: self.frames + 3 });
        }
    }
}
