//! One gameplay step: the clock, physics, scripts and their logs.

use floptle_core::Matter;
use floptle_core::math::DVec3;
use floptle_core::math::Vec3;
use floptle_core::transform::Transform;
use std::collections::HashMap;
use floptle_core::time::Instant;
use crate::{Editor, FOCUS_SECS, anim, grab_cursor};

use crate::perf_readout::fifo_pacing_multiple;

/// What one frame's script pass settles before the scripts run, and what
/// the later phases of the step read back.
struct FrameFeed {
    /// The script delta: `dt`, or 0 while paused.
    sdt: f32,
    /// The project's scripts folder.
    dir: std::path::PathBuf,
    /// The active camera's yaw and pitch, riding every input snapshot.
    aim: Option<[f32; 2]>,
    /// The frame's input snapshot, restored after the tick loop overwrites it.
    frame_input: floptle_script::InputSnapshot,
}

impl Editor {
    /// Add a subsystem's cost to this frame.
    ///
    /// A one-line helper because the alternative is `self.script_host.profile()
    /// .borrow_mut().record(...)` at every measured site, and a borrow that long
    /// spelled out eight times is eight chances to hold it across something that
    /// also wants it.
    pub(crate) fn profile_record(&self, bucket: floptle_core::profile::Bucket, ms: f32) {
        self.script_host.profile().borrow_mut().record(bucket, ms);
    }

    /// Live syntax check for the active IDE file (drives the red squiggle):
    /// Lua through the script host, `.flsl` through the shader compiler.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn check_active_script_syntax(&mut self) {
        self.ide_diag = self.ide.active.and_then(|i| self.ide.open.get(i)).and_then(|f| {
            if f.path.ends_with(".lua") {
                self.script_host.check_syntax(&f.text)
            } else if crate::assets::is_shader(&f.path) {
                Editor::check_flsl_syntax(&f.text)
            } else {
                None
            }
        });
    }

    /// Frame-time smoothing: snap the measured dt to the nearest whole multiple
    /// of the display's refresh period when it's close. Under vsync (Fifo) a
    /// frame's true screen time is a whole number of refresh periods — the
    /// CPU-side measurement just adds 1–3 ms of scheduler noise on top, and
    /// feeding that noise into the fixed-step accumulator moves everything the
    /// interpolation renders by `velocity × noise` every frame (the moving-
    /// jitter that came and went with window mode / load). The residual error
    /// is banked and folded back a hair per frame, so long-term time stays
    /// wall-clock exact (a fast clip on the bank absorbs one-off stalls).
    pub(crate) fn smooth_dt(&mut self, raw: f32) -> f32 {
        // (Re)read the monitor's refresh rate occasionally — cheap, and the
        // window can move between monitors.
        if self.refresh_poll == 0 {
            // 60 frames rather than 240. The poll interval is also the length of
            // every outage below, so a long one turned a momentary hiccup into
            // seconds of unsnapped dt.
            self.refresh_poll = 60;
            self.reread_refresh_period();
        }
        self.refresh_poll -= 1;
        let period = self.refresh_period;
        if period <= 0.0 || raw <= 0.0 {
            self.dt_snap_rate *= 0.99;
            return raw;
        }
        let n = (raw / period).round();
        // Snap only when the measurement is close to a whole vsync count (and
        // at least one) — a giant hitch or an uncapped frame passes through.
        if n < 1.0 || (raw - n * period).abs() > period * 0.12 {
            self.dt_snap_error = self.dt_snap_error.clamp(-period, period);
            // A miss is not a fault — an uncapped frame legitimately misses. A
            // long run of them means the snap is inert, which is a different
            // thing and worth being able to see.
            self.dt_snap_rate *= 0.99;
            return raw;
        }
        self.dt_snap_rate = self.dt_snap_rate * 0.99 + 0.01;
        self.dt_snap_error += raw - n * period;
        // Fold the banked truth back in slowly (≤0.25 ms/frame): time stays
        // exact without re-introducing per-frame noise.
        let give = self.dt_snap_error.clamp(-0.00025, 0.00025);
        self.dt_snap_error -= give;
        n * period + give
    }

    /// Advance the frame clock: `dt`/`elapsed`, the editor fly-camera (unless
    /// the Game view owns input), the smoothed FPS title, and the F-key focus
    /// glide. Returns `(dt, elapsed)`.
    pub(crate) fn advance_clock(&mut self, game_focused: bool) -> (f32, f32) {
        let now = Instant::now();
        let raw_dt = self.last.map(|l| (now - l).as_secs_f32()).unwrap_or(0.0);
        self.last = Some(now);
        let dt = self.smooth_dt(raw_dt);
        // This frame's time for UI style transitions. Handed to the runtime
        // rather than to a pass, because several passes style the same tree in
        // a frame and each needs the real `dt`; `begin_frame` is what stops an
        // element charging it twice (see `Editor::ui_style_dt`).
        self.ui_style_dt = dt.min(0.25);
        self.ui_style_rt.begin_frame();
        // The ◫ UI tab's canvas runs its own style runtime (so a previewed
        // hover there can't fight the Game view's real one) and therefore needs
        // its own clock. Read, not drained: it renders exactly once a frame.
        self.ui_design_dt = dt.min(0.25);
        self.ui_frame_dt = dt.min(0.25);
        let elapsed = self.started.map(|s| (now - s).as_secs_f32()).unwrap_or(0.0);
        self.poll_ui_styles(elapsed);
        self.fog_time = elapsed; // drifts the volumetric-fog noise (offscreen views too)
        // Don't drive the editor (Scene) camera while the Game viewport is focused — that
        // input belongs to the game (e.g. the mouse is over the Game view in split mode).
        if !game_focused {
            self.camera.update(&self.input, dt);
            // Mouse wheel dollies the editor camera when hovering the Scene viewport.
            // Consumed here (before scripts / finish_input_frame): scripts only read
            // scroll while the Game view is focused, so this never steals game input.
            if self.input_scroll != 0.0 && self.cursor_over_scene() {
                self.camera.dolly(self.input_scroll);
                self.input_scroll = 0.0;
            }
        }

        // FPS in the window title (smoothed, refreshed a few times a second).
        if dt > 0.0 {
            // **Smooth the frame time and invert at display time.** Smoothing
            // `1.0 / dt` averages a reciprocal, which is biased toward the fast
            // frames — and the more bimodal the distribution, the more wildly it
            // flatters. See `Editor::fps` for the capture where it read 4312 fps
            // against a true 144.
            let ms = dt * 1000.0;
            self.frame_ms = if self.frame_ms > 0.0 { self.frame_ms * 0.9 + ms * 0.1 } else { ms };
            self.fps = 1000.0 / self.frame_ms.max(1e-4);
            self.record_frame_time(ms);
            self.fps_timer += dt;
            if self.fps_timer >= 0.4 {
                self.fps_timer = 0.0;
                if let Some(window) = self.window.as_ref() {
                    // The frame's own cost beside the rate, because they answer
                    // different questions and an fps number alone cannot tell
                    // "this scene is expensive" from "this display is pacing
                    // us". A scene costing 8 ms and presenting at 20 fps is the
                    // second.
                    let cost = (self.frame_ms - self.present_wait_ms).max(0.0);
                    // Reaches the Console too, not only the ⏱ panel and the
                    // title — the panel is opt-in and the title is easy not to
                    // read closely, and this is exactly the report a user who
                    // is not looking for it needs to see.
                    match fifo_pacing_multiple(self.present_wait_ms, cost, self.refresh_period * 1000.0) {
                        Some(n) if n != self.fifo_pacing_warned => {
                            self.fifo_pacing_warned = n;
                            self.console.push(
                                floptle_script::LogLevel::Warn,
                                format!(
                                    "⏱ the DISPLAY is pacing this frame, not the scene: {:.1} ms \
                                     spent waiting on `acquire` every {n}th refresh, while the \
                                     scene itself costs {cost:.1} ms. Try Project Settings ⏵ \
                                     Rendering ⏵ Frame pacing.",
                                    self.present_wait_ms
                                ),
                                None,
                            );
                        }
                        None => self.fifo_pacing_warned = 0,
                        Some(_) => {} // same multiple as last time — already said
                    }
                    // The 1% low beside the mean, because a bimodal frame time
                    // is exactly the distribution that feels worst and the only
                    // one a mean cannot show. The ⏱ panel reports a worst column
                    // for this reason; the title bar is what people actually
                    // read, so it says the same.
                    self.frame_low_ms = self.frame_time_low();
                    let low = self.frame_low_ms;
                    window.set_title(&format!(
                        "Floptle Editor — {}{} — {:.0} fps ({:.1} ms/frame, 1% low {:.1}, cost {:.1}) — {} nodes ({} off screen), {} instances",
                        self.scene_name,
                        if self.scene_dirty { " •" } else { "" },
                        self.fps,
                        self.frame_ms,
                        low,
                        cost,
                        self.render_counts.nodes,
                        self.render_counts.culled,
                        self.render_counts.instances
                    ));
                }
            }
        }

        // Glide an in-progress focus (F). Any wasd/Space/C input hands control back
        // to the user immediately. Only the camera position eases; the view angle is
        // left to mouse-look, so you can look around mid-glide.
        if self.focus_anim.is_some() {
            let moving = self.input.forward
                || self.input.back
                || self.input.left
                || self.input.right
                || self.input.up
                || self.input.down;
            if moving {
                self.focus_anim = None;
            } else {
                let (from, to, t) = {
                    let a = self.focus_anim.as_mut().unwrap();
                    a.t += dt;
                    (a.from, a.to, a.t)
                };
                let k = (t / FOCUS_SECS).clamp(0.0, 1.0);
                let eased = 1.0 - (1.0 - k).powi(3); // ease-out cubic
                self.camera.position = from.lerp(to, eased as f64);
                if k >= 1.0 {
                    self.focus_anim = None;
                }
            }
        }
        (dt, elapsed)
    }

    /// One play-mode step (ordering: scripts → animation → physics): feed body
    /// state / input / assets / animator info to the script host, run the Lua
    /// scripts, apply their writes (models, mouse lock, velocities, heights),
    /// advance the animators, then step the sim. Clears stale script errors
    /// when not playing.
    /// Run a script pass with a wall clock around it, into `Bucket::Scripts`.
    ///
    /// The whole pass, not the hooks in it: the per-instance setup, the ref
    /// resolution, the write flush. The sum of the per-script hook figures
    /// accounts for 5.3 of a real game's 12.9 ms step and hides the rest.
    ///
    /// Net of `Bucket::Mirror`, which the host records from inside `sync_scene`
    /// — nested in this span, so it is subtracted here and the buckets still
    /// sum to the frame instead of counting the mirror twice.
    pub(crate) fn timed_script_pass(&mut self, run: impl FnOnce(&mut Self)) {
        use floptle_core::profile::{Bucket, Span};
        if !self.script_host.profile().borrow().enabled() {
            run(self);
            return;
        }
        let mirror_before = self.script_host.profile().borrow().frame_total(Bucket::Mirror);
        let span = Span::new();
        run(self);
        let ms = span.ms();
        let mut p = self.script_host.profile().borrow_mut();
        let mirror = p.frame_total(Bucket::Mirror) - mirror_before;
        p.record(Bucket::Scripts, (ms - mirror).max(0.0));
    }

    pub(crate) fn play_step(&mut self, dt: f32, game_focused: bool) {
        // **`app.*` is driven from inside the step, not around it.** There are
        // two hosts that run gameplay — the windowed frame and `floptle run`'s
        // headless loop — and a settings API wired into only one of them is a
        // menu that works in the editor and does nothing in a build. Every host
        // reaches gameplay through here, so this is the one place that cannot be
        // half-wired.
        if self.playing {
            self.push_app_info();
        }
        // Play mode: advance the (pausable) script clock and run the Lua scripts
        // attached to nodes. Scripts hot-reload as their files change.
        if self.playing {
            // A scene transition a script queued last frame happens first —
            // at a frame boundary, never mid-frame under the scripts that
            // asked for it (offline/host = switch; joined client = refused).
            for req in std::mem::take(&mut self.pending_scene) {
                let swap = req.is_swap();
                self.perform_scene_request(&req);
                // A full swap ends the queue: the requests behind it named the
                // world that no longer exists, and the new scene's own `start`
                // is the right place to ask for anything else.
                if swap {
                    break;
                }
            }
            let FrameFeed { sdt, dir, aim, frame_input } = self.feed_frame_scripts(dt, game_focused);
            self.run_frame_scripts(sdt, &dir);
            self.tick_loop(sdt, aim, game_focused);
            self.late_pass(sdt, frame_input);
            self.settle_frame(sdt);
        } else if !self.script_errors.is_empty() {
            self.script_errors.clear();
        }
    }

    /// The clock and the facts the frame pass reads: the script delta, the
    /// scripts folder, the camera aim, and the input snapshot the tick loop
    /// later restores.
    fn feed_frame_scripts(&mut self, dt: f32, game_focused: bool) -> FrameFeed {
        // Pausing freezes the clock and the frame delta scripts see, so
        // dt-driven motion stops too (not just `time`-driven motion).
        let sdt = if self.paused { 0.0 } else { dt };
        self.play_t += sdt;
        // Direct field access (not the `scripts_dir()` method) so we don't take
        // a whole-`self` borrow while gpu/egui are mutably borrowed here.
        let dir = self.project_root.join("scripts");
        // Feed the physics body state to scripts so they can read node.grounded and
        // read/write node.vx/vy/vz (a script sets velocity, physics then integrates).
        if let Some(sim) = self.sim.as_ref() {
            let mut states = HashMap::new();
            for r in sim.body_states() {
                states.insert(r.entity.index(), crate::play::body_state(&r));
            }
            for (eid, vel, up, grounded, pos) in sim.compound_states() {
                states.insert(
                    eid,
                    floptle_script::BodyState {
                        vel: [vel.x, vel.y, vel.z],
                        up: [up.x, up.y, up.z],
                        grounded,
                        height: 0.0,
                        pos: [pos.x, pos.y, pos.z],
                        // Compounds resolve contacts per shape with real
                        // impulses; "the floor under it" isn't one normal.
                        ground_normal: None,
                        wall_normal: None,
                    },
                );
            }
            self.script_host.set_bodies(states);
        }
        // The active camera's view angles ride every input snapshot
        // (`input.aimYaw()`): camera-relative movement stays deterministic
        // under prediction because the aim is part of the input command.
        let aim = floptle_core::active_camera(&self.world).map(|e| {
            let wt = floptle_core::world_transform(&self.world, e);
            let (yaw, pitch, _) = wt.rotation.to_euler(floptle_core::math::EulerRot::YXZ);
            [yaw, pitch]
        });
        // Repeaters first: a list whose count changed last frame gets its
        // rows now, so this frame's layout, hit-testing and hooks all see
        // the same set of rows the player is looking at.
        let ui_t = floptle_core::profile::Span::new();
        self.ui_repeaters();
        // Game-UI interaction (buttons + draggable sliders): detect hover/press/
        // click against this frame's layout before scripts run, so a dragged
        // slider's value is already in the ECS when `update` reads it. The hook
        // events dispatch to Lua right after the run.
        self.ui_interact();
        // game UI: repeater expansion, the layout solve and hit-testing.
        self.profile_record(floptle_core::profile::Bucket::Ui, ui_t.ms());
        // Feed the player input to scripts (the Lua `input` API) — but only while the
        // Game view is focused. In the Scene view you're editing, not playing, so the
        // game gets neutral input (the character stops moving) even though physics
        // keeps simulating.
        // …and while the editor is holding the pointer (Escape, with the
        // game still asking for it), the mouse half of that input is the
        // editor's. Freeing the cursor would otherwise be half a fix: the
        // camera script keeps reading raw motion, so the view spins the
        // whole way over to the Inspector and every click on it also
        // reaches the game. Keys keep flowing — the game is still playing.
        let mouse_is_the_editors = self.cursor_freed;
        // …and while the platform's overlay (Steam's Shift+Tab) is up, the
        // keys are the overlay's, on the same neutral-input rule. The sim
        // keeps running — a networked session cannot pause for one
        // player's shopping trip — but a key held through the open is
        // released here rather than stuck down until it's next pressed.
        let input_live = game_focused && !self.script_host.overlay_active();
        let frame_input = if input_live {
            floptle_script::InputSnapshot {
                keys_down: self.input_keys.clone(),
                keys_pressed: self.input_keys_pressed.clone(),
                keys_released: self.input_keys_released.clone(),
                // Whatever a focused text field did not eat.
                typed: self.input_typed.clone(),
                mouse: self.cursor.map(|c| (c.x, c.y)).unwrap_or((0.0, 0.0)),
                mouse_delta: if mouse_is_the_editors {
                    (0.0, 0.0)
                } else {
                    self.input_mouse_delta
                },
                scroll: if mouse_is_the_editors { 0.0 } else { self.input_scroll },
                buttons_down: if mouse_is_the_editors {
                    [false; 3]
                } else {
                    self.input_buttons
                },
                buttons_pressed: if mouse_is_the_editors {
                    [false; 3]
                } else {
                    self.input_buttons_pressed
                },
                aim,
            }
        } else {
            floptle_script::InputSnapshot { aim, ..Default::default() }
        };
        self.script_host.set_input(frame_input.clone());
        // …and the action layer's frame domain, off the same devices and
        // the same focus rule, so `input.action(...)` and `input.key(...)`
        // agree about whether the game is being played this frame.
        self.resolve_frame_actions(sdt, input_live);
        // Lend the sim's colliders to scripts so `raycast(...)` works this frame
        // (physics doesn't step until after scripts, so this is safe). The sim
        // origin rides along so ray coordinates convert world ↔ sim frame.
        if let Some(sim) = self.sim.as_mut() {
            self.script_host
                .set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
        }
        // …and the dynamic bodies' hulls (copies), so rays can hit
        // players/crates and identify the node (`hit.node`).
        if let Some(sim) = self.sim.as_ref() {
            self.script_host.set_hulls(sim.body_hulls(&self.world));
        }
        // Lend the asset root (for `assets.getFile/getContents`) and the material
        // presets (so `node.material = "Gold"` resolves) for this frame's scripts.
        self.script_host.set_project_root(self.project_root.clone());
        // The running scene's name, for `scene.current()`.
        self.script_host.set_scene_name(&self.scene_name);
        // The scene's bodies of water, in world coordinates — `water.depthAt`
        // answers the same question the solver does, from the same geometry,
        // so a swim state can never disagree with the physics floating it.
        self.script_host.set_water_volumes(crate::shading::water_infos(&self.world));
        self.script_host.set_materials(
            self.materials.iter().map(|(n, d)| (n.clone(), d.to_material())).collect(),
        );
        // Feed each animator's state (layers/current/time) so scripts can read
        // anim:state()/:time()/:clips() this frame.
        self.script_host.set_anim_info(anim::build_info(&self.anim));
        // Feed each particle node's state so scripts can read
        // node:particles():isPlaying()/:alive() this frame.
        self.script_host.set_vfx_info(self.vfx.script_info(&self.world));
        // Feed sound playback state so scripts can read sound:isPlaying()/
        // :position() this frame.
        #[cfg(feature = "devices")]
        self.script_host.set_audio_info(self.audio.script_info());
        // Feed each assembly's live compound state (`assembly.info`).
        self.feed_assembly_info();
        FrameFeed { sdt, dir, aim, frame_input }
    }

    /// The frame pass: `update` and the UI hooks, then what the scripts
    /// asked for — mouse lock, scene loads, captions, frame steps, animation,
    /// velocity writes, assembly and terrain commands.
    fn run_frame_scripts(&mut self, sdt: f32, dir: &std::path::Path) {
        self.timed_script_pass(|s| s.script_host.run(&mut s.world, dir, sdt, s.play_t));
        // UI hook events (clicked / hoverStart / …) fire against the run's
        // fresh scene mirror, with their own write flush.
        let ui_events = std::mem::take(&mut self.ui_events);
        self.script_host.run_ui_hooks(&mut self.world, &ui_events);
        self.script_errors = self.script_host.errors().to_vec();
        // Apply any mouse lock/unlock a script requested this frame (grab + hide the
        // cursor for free-look, or release it). The state persists until changed/Stop.
        // Deduped against the current state: shipped camera scripts call
        // setMouseLocked every frame from update(), and re-issuing the OS grab at
        // frame rate tears down/recreates the pointer lock each time (on Wayland
        // that reads as a flickering, uncontrollable cursor).
        if let Some(want) = self.script_host.take_mouse_lock() {
            // An explicit unlock is a game saying "the pointer is mine now",
            // and it has to release the editor's click-to-play trap too —
            // that is a second, invisible lock owner the game has no way to
            // reach. Deduping it against `script_mouse_lock` would drop the
            // call entirely in the case that matters, because a game opening
            // a menu never locked the mouse in the first place.
            let freed_trap = !want && std::mem::take(&mut self.game_trap);
            // …and it ends any editor override, because there is nothing
            // left to override: the game and the editor now agree that the
            // pointer is loose, and a game that opens its own menu two
            // minutes after you pressed Escape must get its clicks.
            if !want {
                self.cursor_freed = false;
            }
            if want != self.script_mouse_lock {
                self.script_mouse_lock = want;
                // While the editor is holding the pointer, the game's wish
                // is recorded and not applied — it lands the moment you
                // click back into the Game view. This is the whole reason
                // Escape works against a camera script that re-locks every
                // frame: the re-lock is a no-op until you say so.
                if !self.cursor_freed
                    && let Some(window) = self.window.as_ref()
                {
                    self.cursor_lock_soft = grab_cursor(window, want);
                }
            } else if freed_trap
                && let Some(window) = self.window.as_ref()
            {
                // Nothing changed as far as the script flag goes, but the
                // trap was the one holding the OS grab — let it go.
                self.cursor_lock_soft = grab_cursor(window, false);
            }
        }
        // A `scene.load(...)` from this frame's scripts: queued, performed
        // at the top of the next frame (see above).
        // `ui.focus(node)` from a script. Applied after the run so the
        // hooks fire on the next frame's pass, in the same place engine-
        // driven focus changes fire them — one code path, one ordering.
        if let Some(want) = self.script_host.take_ui_focus_request() {
            self.ui_focus_set(want);
        }
        self.pending_scene.extend(self.script_host.take_scene_requests());
        // Accessibility: the settings a game's options menu
        // wrote this frame come back out, and the captions it asked for join
        // the on-screen queue. Read after the run so a menu that changes text
        // scale is honoured by the very next layout.
        self.access = self.script_host.access();
        // Captions age out on their own — a line nobody removed is a line
        // covering the game.
        for c in &mut self.captions {
            c.1 -= sdt;
        }
        self.captions.retain(|c| c.1 > 0.0);
        for c in self.script_host.take_captions() {
            // Newest last, and a modest cap: captions are read in order, and a
            // game spamming them would otherwise cover its own screen.
            self.captions.push((c.text, c.seconds));
            if self.captions.len() > 4 {
                self.captions.remove(0);
            }
        }
        // `water.setFrozen(node, on)` — freezing is a state, so it lands on
        // the node and the physics field is rebuilt from it. The rebuild
        // preserves live velocities, so a sea freezing under a swimmer does
        // not fling them.
        let freezes = self.script_host.take_water_freezes();
        if !freezes.is_empty() {
            let mut changed = false;
            for (eid, on) in freezes {
                let Some(e) =
                    self.world.entity_with::<Matter>(eid)
                else {
                    continue;
                };
                match self.world.get_mut::<Matter>(e) {
                    Some(Matter::WaterVolume { frozen, .. }) => {
                        changed |= *frozen != on;
                        *frozen = on;
                    }
                    _ => self.console.push(
                        floptle_script::LogLevel::Warn,
                        "water.setFrozen: that node is not a Water Volume".into(),
                        None,
                    ),
                }
            }
            if changed {
                self.rebuild_sim();
            }
        }
        // GPU-load any models a script swapped via `node.model` (the Matter is
        // already updated by run; re-importing here means the new mesh renders
        // this frame).
        self.load_script_swapped_models();
        // `physics.step([n])` from a script — the same frame-stepper as ⏭. Drained
        // in the frame pass, not inside the tick loop: once the tick is frozen that
        // loop doesn't run, so a request drained there could never be the thing that
        // unfreezes it. And before animation, so the step it releases advances the
        // pose on the same frame as the gameplay tick rather than one behind.
        let steps = self.script_host.take_frame_steps();
        if steps > 0 {
            self.step_tick(steps);
        }
        // Animation: bind + apply queued Lua animator commands + advance every
        // controller (ordering: scripts → animation → physics), then dispatch
        // fired clip events back into the node's scripts.
        let anim_cmds = self.script_host.take_anim_commands();
        // A frame-step advances animation by exactly the ticks it is about to
        // release (the tick loop consumes `tick_steps` further down), so the pose
        // you are looking at belongs to the gameplay frame you stopped on. Paused
        // with no step pending, `sdt` is already 0.
        let anim_dt = if self.paused {
            self.tick_steps as f32 * self.game_tick.step
        } else {
            sdt
        };
        let anim_t = floptle_core::profile::Span::new();
        let fired = anim::advance_animators(
            &mut self.anim,
            &mut self.world,
            &self.mesh_registry,
            anim_dt,
            anim_cmds,
        );
        // Animation: clip sampling, blending, pose composition and CPU
        // skinning.
        self.profile_record(floptle_core::profile::Bucket::Animation, anim_t.ms());
        for (eid, func) in fired {
            self.script_host.call_function(&mut self.world, eid, &func);
        }
        // Animator warnings (e.g. play() on a state name the controller
        // doesn't have) surface in the Console, once per name.
        for msg in self.anim.warnings.drain(..) {
            self.console.push(floptle_script::LogLevel::Warn, msg, None);
        }
        // Event handlers can log/raise — surface those in the Scripting tab
        // (run() cleared + snapshotted errors before the dispatch above).
        if !self.script_host.errors().is_empty() {
            self.script_errors = self.script_host.errors().to_vec();
        }
        // Apply script velocity writes, then run the gameplay tick loop (docs/
        // netcode-design.md §3): each banked 60 Hz tick runs `fixedUpdate` with a
        // per-tick input snapshot, applies its writes, and steps physics exactly one
        // tick — the deterministic unit netcode snapshots/prediction share. Rendered
        // transforms interpolate across the current tick (anti-stutter). Gravity is
        // rebuilt from the scene's GravityVolume node(s) every frame (cheap scan) so
        // tweaking mode/strength/radius takes effect immediately. The active camera
        // is the floating-origin focus: drift far enough and the sim recenters on it.
        if let Some(sim) = self.sim.as_mut() {
            sim.world.gravity = Self::build_gravity_field(&self.world, sim.world.origin);
            // Water is rebuilt every frame for the same reason gravity is:
            // a WaterVolume spawned, moved, resized,
            // disabled or destroyed while the game is running must be in
            // the solver's field the same frame it is in the renderer's —
            // `water_draw` already gathers from the live world every
            // frame, so the two were disagreeing about *when* a pool
            // exists, not about what it is. Same cost shape as gravity's
            // scan (cheap on a level of a few thousand nodes); "static
            // per step" (the doc comment on `PhysicsWorld::water`) is a
            // claim about determinism within one tick, not about being
            // built once per session.
            sim.world.water = Self::build_water_field(&self.world, sim.world.origin);
            sim.world.set_colliders(self.script_host.take_colliders()); // reclaim before stepping
            // Live Inspector edits: re-read RigidBody tunables (shape/size, friction,
            // restitution, gravity, pos/rot locks) into the running bodies each frame —
            // no teleport.
            sim.sync_dynamic_params(&self.world);
            // `update`'s velocity/height writes apply before the first tick, so
            // frame-pass controllers (the pre-fixedUpdate style) behave as before.
            for (eid, v) in self.script_host.take_body_changes() {
                sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
            }
            for (eid, h) in self.script_host.take_body_height_changes() {
                sim.set_body_height(eid, h);
            }
            for (eid, p) in self.script_host.take_body_pos_changes() {
                sim.set_body_position(eid, DVec3::new(p[0], p[1], p[2]));
            }
        }
        // Assembly commands from the frame pass (`assembly.forceAt` in
        // `update`, splits from UI handlers): forces arm the coming ticks,
        // splits happen now.
        self.drain_assembly_cmds();
        // Terrain edits queued by the frame pass (`terrain.sculpt/dig/...`):
        // applied to the authority field + the sim's collider copy before any
        // tick steps, so physics never disagrees with the surface.
        self.drain_script_terrain_ops();
    }

    /// The gameplay tick loop: fixed steps against the accumulated clock,
    /// each with its own input snapshot, then the interpolated writeback.
    fn tick_loop(&mut self, sdt: f32, aim: Option<[f32; 2]>, game_focused: bool) {
        // tweaking mode/strength/radius takes effect immediately. The active camera
        // is the floating-origin focus: drift far enough and the sim recenters on it.
        let focus = floptle_core::active_camera(&self.world)
            .map(|e| floptle_core::world_transform(&self.world, e).translation);
        if self.sim.is_some() {
            self.game_tick.accumulate(sdt);
            // Frame-step. While frozen the clock banks nothing (so unpausing can
            // never release a burst of caught-up ticks) and `tick_steps` is the only
            // thing that lets a tick through. Draining the accumulator also drops
            // `alpha` to 0, so what you look at between steps is the tick pose
            // itself, not an interpolated one — which is the whole point of
            // stopping on a frame.
            let stepping = self.paused;
            if stepping {
                self.game_tick.reset();
            } else {
                // Steps queued while running are meaningless — never let them bank.
                self.tick_steps = 0;
            }
            loop {
                if stepping {
                    if self.tick_steps == 0 {
                        break;
                    }
                    self.tick_steps -= 1;
                } else if !self.game_tick.tick() {
                    break;
                }
                self.game_tick_no += 1;
                // Celestial rails first (solar demo S2): body nodes + their
                // terrain collider anchors + gravity centers + the space.*
                // snapshot all reflect this tick before scripts and physics.
                self.update_space_rails(self.game_tick.step as f64);
                // Per-tick input: consume the tick accumulators (edges bank between
                // ticks so a between-tick press is never lost). Neutral when the
                // Game view isn't focused — but still consumed, so stale edges
                // don't fire on refocus.
                let snap = if game_focused {
                    // The accumulators are drained either way — while the
                    // editor holds the pointer the mouse half is dropped
                    // rather than banked, so nothing fires in a burst the
                    // moment you hand the cursor back.
                    let (dx, dy) = std::mem::take(&mut self.tick_mouse_delta);
                    let wheel = std::mem::take(&mut self.tick_scroll);
                    let pressed = std::mem::take(&mut self.tick_buttons_pressed);
                    let mine = self.cursor_freed;
                    floptle_script::InputSnapshot {
                        keys_down: self.input_keys.clone(),
                        keys_pressed: std::mem::take(&mut self.tick_keys_pressed),
                        keys_released: std::mem::take(&mut self.tick_keys_released),
                        typed: std::mem::take(&mut self.tick_typed),
                        mouse: self.cursor.map(|c| (c.x, c.y)).unwrap_or((0.0, 0.0)),
                        mouse_delta: if mine { (0.0, 0.0) } else { (dx, dy) },
                        scroll: if mine { 0.0 } else { wheel },
                        buttons_down: if mine { [false; 3] } else { self.input_buttons },
                        buttons_pressed: if mine { [false; 3] } else { pressed },
                        aim,
                    }
                } else {
                    self.tick_keys_pressed.clear();
                    self.tick_keys_released.clear();
                    self.tick_typed.clear();
                    self.tick_mouse_delta = (0.0, 0.0);
                    self.tick_scroll = 0.0;
                    self.tick_buttons_pressed = [false; 3];
                    floptle_script::InputSnapshot { aim, ..Default::default() }
                };
                // Keep what the scripts saw: prediction records + ships it.
                self.last_tick_input = snap.clone();
                self.script_host.set_input(snap);
                // The action layer's tick domain — the one with input
                // history, so motions and buffers advance exactly once per
                // tick regardless of framerate. Drains the banked edges.
                //
                // A rollback session owns that domain instead: every peer's
                // input, including ours, is written into its slot at its
                // Applied tick and history advances exactly once from
                // there. Resolving devices here as well would advance it a
                // second time and halve every motion window on the local
                // player only (see `InputSystem::sample_tick`). The driver
                // also runs its fighters' hooks and steps their bodies —
                // the rest of this tick then runs for everything else.
                if self.net_rollback.is_some() {
                    self.net_rollback_tick(game_focused);
                } else {
                    self.resolve_tick_actions(self.game_tick.step, game_focused);
                }
                if let Some(sim) = self.sim.as_mut() {
                    // Fresh body state for this tick (post previous tick's physics).
                    let mut states = HashMap::new();
                    for r in sim.body_states() {
                        states.insert(r.entity.index(), crate::play::body_state(&r));
                    }
                    for (eid, vel, up, grounded, pos) in sim.compound_states() {
                        states.insert(
                            eid,
                            floptle_script::BodyState {
                                vel: [vel.x, vel.y, vel.z],
                                up: [up.x, up.y, up.z],
                                grounded,
                                height: 0.0,
                                pos: [pos.x, pos.y, pos.z],
                                ground_normal: None,
                                wall_normal: None,
                            },
                        );
                    }
                    self.script_host.set_bodies(states);
                    // Lend colliders so `raycast(...)` works inside `fixedUpdate` too.
                    self.script_host.set_colliders(
                        std::mem::take(&mut sim.world.colliders),
                        sim.world.origin,
                    );
                    self.script_host.set_hulls(sim.body_hulls(&self.world));
                }
                // `time` on the fixed pass is the deterministic tick clock.
                let tick_time = self.game_tick_no as f32 * self.game_tick.step;
                // Real hosting: each remote player's Predicted node runs
                // with its owner's replayed input for this tick — the
                // one-script model (§6), server side. Those nodes are
                // filtered out of the global passes; run_*_for bypasses
                // the filters. The host's own input is restored after.
                if !self.net_remote_predicted.is_empty() && self.net_server.is_some() {
                    if let Some(s) = self.net_server.as_mut() {
                        // Tick-start pump so this tick's freshest client
                        // inputs are in the buffer before scripts consume.
                        s.pump_server(&self.world, self.game_tick_no);
                    }
                    for (e, owner) in self.net_remote_predicted.clone() {
                        let Some(s) = self.net_server.as_mut() else { break };
                        let inp = s.input_for(owner, self.game_tick_no);
                        crate::input_actions::apply_net_input_to(&self.script_host, &inp);
                        let eix = e.index();
                        self.timed_script_pass(|s| {
                            s.script_host.run_frame_for(
                                &mut s.world,
                                eix,
                                s.game_tick.step,
                                tick_time,
                            );
                            s.script_host.run_fixed_for(
                                &mut s.world,
                                eix,
                                s.game_tick.step,
                                tick_time,
                            );
                        });
                    }
                    self.script_host.set_input(self.last_tick_input.clone());
                }
                // A predicted node's `update` rides the tick clock (its
                // frame pass is filtered) so client + server integrate the
                // same controller identically — see net.rs.
                if let Some((pe, _)) = &self.net_predictor {
                    let pe = pe.index();
                    self.timed_script_pass(|s| {
                        s.script_host.run_frame_for(&mut s.world, pe, s.game_tick.step, tick_time)
                    });
                }
                self.feed_assembly_info();
                self.timed_script_pass(|s| {
                    s.script_host.run_fixed(&mut s.world, s.game_tick.step, tick_time)
                });
                if let Some(sim) = self.sim.as_mut() {
                    sim.world.set_colliders(self.script_host.take_colliders()); // reclaim
                    // Apply the tick's writes, then step physics exactly one tick.
                    sim.sync_dynamic_params(&self.world);
                    for (eid, v) in self.script_host.take_body_changes() {
                        sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                    }
                    for (eid, h) in self.script_host.take_body_height_changes() {
                        sim.set_body_height(eid, h);
                    }
                    for (eid, p) in self.script_host.take_body_pos_changes() {
                        sim.set_body_position(eid, DVec3::new(p[0], p[1], p[2]));
                    }
                }
                // `fixedUpdate`'s assembly thrust arms this tick's substeps.
                self.drain_assembly_cmds();
                // This tick's terrain edits (`fixedUpdate` digs) land before the
                // step: the tick that dug the hole also falls into it.
                self.drain_script_terrain_ops();
                // Bound crash loss on `save.*` data: flush every ~5 s of ticks
                // (a clean no-op while the store is unchanged).
                if self.game_tick_no.is_multiple_of(300) {
                    self.script_host.flush_save();
                }
                // `physics.pause(on)` gates the whole physics step (scripts,
                // rails and streaming keep running — loading screens hold
                // the world still while it assembles). Queued held forces
                // are dropped, not banked: unpausing must not fire a burst
                // of accumulated thrust.
                if let Some(on) = self.script_host.take_physics_pause_request() {
                    self.physics_paused = on;
                    self.script_host.set_physics_paused(on);
                }
                if let Some(sim) = self.sim.as_mut() {
                    if self.physics_paused {
                        sim.clear_held_forces();
                    } else {
                        // Physics. Timed per tick and
                        // accumulated, because a frame can run several — a
                        // per-frame timer would report the last tick and hide
                        // a catch-up frame, which is exactly the spike a game
                        // notices.
                        let t = floptle_core::profile::Span::new();
                        sim.step_tick(self.game_tick.step, focus);
                        let ms = t.ms();
                        self.script_host
                            .profile()
                            .borrow_mut()
                            .record(floptle_core::profile::Bucket::Physics, ms);
                    }
                }
                // Collision / trigger events from this tick, dispatched to
                // both nodes' scripts: `onCollisionEnter/Stay/Exit(node,
                // other, hit)` for solid contacts (incl. body-vs-body),
                // `onTriggerEnter/Stay/Exit` when a Trigger collider is
                // involved. Events fire where physics runs (offline, the
                // server, a predicted owner) — never during replays.
                let touches =
                    self.sim.as_mut().map(|s| s.take_touch_events()).unwrap_or_default();
                for ev in touches {
                    use floptle_physics::TouchPhase;
                    let func = match (ev.sensor, ev.phase) {
                        (true, TouchPhase::Enter) => "onTriggerEnter",
                        (true, TouchPhase::Stay) => "onTriggerStay",
                        (true, TouchPhase::Exit) => "onTriggerExit",
                        (false, TouchPhase::Enter) => "onCollisionEnter",
                        (false, TouchPhase::Stay) => "onCollisionStay",
                        (false, TouchPhase::Exit) => "onCollisionExit",
                    };
                    let p = [ev.point.x, ev.point.y, ev.point.z];
                    let n = [ev.normal.x, ev.normal.y, ev.normal.z];
                    self.script_host.call_touch(&mut self.world, ev.a, func, ev.b, p, n);
                    self.script_host.call_touch(&mut self.world, ev.b, func, ev.a, p, n);
                }
                // A handler's body writes (knockback, bounce) land this
                // tick, not the next one.
                if let Some(sim) = self.sim.as_mut() {
                    for (eid, v) in self.script_host.take_body_changes() {
                        sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                    }
                }
                // Netcode rides the tick (docs/multiplayer.md §9): session
                // commands, server snapshot send, ghost-client apply, RPC/event
                // dispatch — all after physics, all on the deterministic clock.
                self.net_tick(self.game_tick_no);
            }
            if let Some(sim) = self.sim.as_mut() {
                // Render this frame partway into the current tick: smooth at any fps.
                sim.writeback_interpolated(&mut self.world, self.game_tick.alpha());
            }
            // Prediction corrections render as a decaying nudge, not a snap:
            // the rendered transform carries the (shrinking) error offset.
            if let Some((pe, pred)) = &self.net_predictor
                && pred.error_offset != [0.0; 3]
                && let Some(tr) = self.world.get_mut::<Transform>(*pe)
            {
                tr.translation +=
                    floptle_core::math::DVec3::from_array(pred.error_offset);
            }
        }
    }

    /// `lateUpdate`: the camera pass after physics, then the immediate-mode
    /// draw and gizmo queues.
    fn late_pass(&mut self, sdt: f32, frame_input: floptle_script::InputSnapshot) {
        // `lateUpdate` — the camera pass: after physics and the interpolated
        // writeback, so followers sample this frame's final poses. (A camera
        // positioned in `update` reads last frame's pose — a follow error of
        // velocity × dt that turns frame-time noise into visible jitter.)
        // The tick loop overwrote the input snapshot with per-tick state —
        // restore the frame snapshot first, so mouse/scroll reads in
        // lateUpdate see this frame's input, not the last tick's leftovers.
        self.script_host.set_input(frame_input);
        // Re-lend the sim's state for the late pass: the tick loop reclaimed
        // the colliders before stepping, so without this an orbit camera's
        // wall raycast would see no static geometry. Hulls and body state are
        // refreshed too — post-step, so `raycast` hits bodies where they
        // rendered and `node.vx/grounded` reads this frame's final values.
        if let Some(sim) = self.sim.as_mut() {
            let mut states = HashMap::new();
            for r in sim.body_states() {
                states.insert(r.entity.index(), crate::play::body_state(&r));
            }
            // Compound roots read like bodies too (node.vx / up_x /
            // grounded on a vessel) — before the collider hand-off, since
            // their gravity-up needs the collider set.
            for (eid, vel, up, grounded, pos) in sim.compound_states() {
                states.insert(
                    eid,
                    floptle_script::BodyState {
                        vel: [vel.x, vel.y, vel.z],
                        up: [up.x, up.y, up.z],
                        grounded,
                        height: 0.0,
                        pos: [pos.x, pos.y, pos.z],
                        // Compounds resolve contacts per shape with real
                        // impulses; "the floor under it" isn't one normal.
                        ground_normal: None,
                        wall_normal: None,
                    },
                );
            }
            self.script_host.set_bodies(states);
            self.script_host
                .set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
        }
        if let Some(sim) = self.sim.as_ref() {
            self.script_host.set_hulls(sim.body_hulls(&self.world));
        }
        self.timed_script_pass(|s| s.script_host.run_late(&mut s.world, sdt, s.play_t));
        if let Some(sim) = self.sim.as_mut() {
            sim.world.set_colliders(self.script_host.take_colliders()); // reclaim
            // A velocity write from lateUpdate still lands (applied next
            // step) — but the camera pass shouldn't steer bodies; drain so
            // nothing double-applies with next frame's `update` writes.
            for (eid, v) in self.script_host.take_body_changes() {
                sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
            }
        }
        // Surface fixedUpdate errors alongside the frame pass's.
        if !self.script_host.errors().is_empty() {
            self.script_errors = self.script_host.errors().to_vec();
        }
        // Immediate-mode 3D lines queued this frame — by `update`, `fixedUpdate`
        // and `lateUpdate` — drained once per frame, replacing the list (an
        // idle script clears its lines). Drained here, after the late pass,
        // so a camera-pass drawer (the solar map) lands the same frame as the
        // camera it positioned — draining per tick left the lines a frame
        // behind an interpolated camera.
        self.script_lines = self.script_host.take_draw_lines();
        self.script_tris = self.script_host.take_draw_tris();
        // Textured quads: each texture path resolves through the registry here,
        // while `self` is ours to borrow mutably (the render pass only reads), and
        // the list is grouped by texture. A path that will not load is dropped,
        // not drawn white.
        let quads = self.script_host.take_draw_quads();
        self.script_quads.clear();
        for q in quads {
            if let Some(id) = self.ensure_texture(&q.texture) {
                self.script_quads.push((id, q));
            }
        }
        self.script_quads.sort_by_key(|(id, _)| id.0);
        self.script_rects = self.script_host.take_draw_rects();
        self.script_texts = self.script_host.take_draw_texts();
        // Script debug gizmos queued this frame — by `update` and `fixedUpdate` —
        // drained once here (drawn by the viewport overlay), plus the multiplayer
        // harness's ghost-client markers.
        self.script_gizmos = self.script_host.take_gizmos();
        self.net_ghost_gizmos();
    }

    /// The end of the step: spawns, scatter, attachments, particles, audio,
    /// and the `app.*` requests.
    fn settle_frame(&mut self, sdt: f32) {
        // Prefab spawns + node destroys scripts queued this frame — applied
        // before attachments/particles so a spawned node is complete (body,
        // meshes, callback-configured) within this same frame.
        self.apply_script_spawns();
        // Scatter prototypes: resolved here, before the frame's GPU borrow,
        // because baking a prefab imports models and that needs `&mut self`.
        self.bake_scatter_prototypes();
        // Bone attachments resolve after physics: physics moves the mesh root (a
        // character body), while animation only bent the bones — so a weapon on a
        // bone must read the post-physics mesh world or it swims a frame behind.
        anim::resolve_attachments(&self.anim, &mut self.world, &self.mesh_registry);
        // 2D cameras follow after all of that, for the same reason bone
        // attachments do: a camera chasing a player has to read where the
        // player ended up this frame, not where they started it.
        floptle_core::camera2d::step_all(&mut self.world, sdt, self.play_t as f64);
        // Particles tick last: emitter node transforms are final for the frame
        // (scripts → animation → physics → attachments → particles). Apply any
        // play/stop/restart a script queued this frame first, so it lands now.
        // everything from here to the end of `advance` is the
        // particles bucket. It had no producer at all, so `perf.ms("particles")`
        // answered a confident 0.0 while collection was on — which reads as
        // "particles are free", the one answer a profiler must never give by
        // accident.
        let vfx_t = floptle_core::profile::Span::new();
        let vfx_cmds = self.script_host.take_vfx_commands();
        self.vfx.apply_script_commands(&self.world, vfx_cmds);
        // Fire-and-forget one-shots a script requested this frame (spawnEffect).
        for (key, p, v) in self.script_host.take_spawn_effects() {
            let vel = floptle_core::math::Vec3::new(v[0] as f32, v[1] as f32, v[2] as f32);
            self.vfx.spawn_detached(&key, floptle_core::math::DVec3::from_array(p), vel);
        }
        // Hand particles the live gravity field so `GravityMode::Field` effects fall
        // toward planets (same field the rigidbodies use), not world −Y.
        let vfx_grav = self.sim.as_ref().map(|s| crate::vfx::VfxGravity {
            field: &s.world.gravity,
            colliders: &s.world.colliders,
            origin: s.world.origin,
        });
        self.vfx.advance(&self.world, sdt, vfx_grav);
        self.profile_record(floptle_core::profile::Bucket::Particles, vfx_t.ms());
        // Audio: apply queued Lua commands, then tick voices against the
        // final node transforms (same ordering rationale as particles).
        let audio_t = floptle_core::profile::Span::new();
        let audio_cmds = self.script_host.take_audio_commands();
        let root = self.project_root.clone();
        if !audio_cmds.is_empty() {
            // `node:sound():setClip(...)` mutates the component (a string —
            // outside the numeric mirror); the diff in advance() restarts
            // the voice on the new clip.
            for cmd in &audio_cmds {
                if let floptle_script::AudioCmd::SourceSetClip { ent, clip } = cmd {
                    let target = self
                        .world
                        .query::<floptle_audio::AudioSource>()
                        .find(|(e, _)| e.index() == *ent)
                        .map(|(e, _)| e);
                    if let Some(e) = target
                        && let Some(src) = self.world.get_mut::<floptle_audio::AudioSource>(e)
                    {
                        src.clip = clip.clone();
                    }
                }
            }
            #[cfg(feature = "devices")]
            self.audio.apply_script_commands(&self.world, &root, audio_cmds);
            #[cfg(not(feature = "devices"))]
            let _ = (&root, audio_cmds);
        }
        // Listener = the active camera's ears.
        let listener = floptle_core::active_camera(&self.world)
            .map(|e| {
                let wt = floptle_core::world_transform(&self.world, e);
                floptle_audio::Listener {
                    position: wt.translation,
                    forward: (wt.rotation * floptle_core::math::Vec3::NEG_Z).as_dvec3(),
                    right: (wt.rotation * floptle_core::math::Vec3::X).as_dvec3(),
                }
            })
            .unwrap_or_default();
        #[cfg(feature = "devices")]
        for e in self.audio.advance(&self.world, &root, listener) {
            // EndBehavior::Destroy — the sound finished, its node goes too.
            self.world.despawn(e);
            self.selection.retain(|s| *s != e);
        }
        #[cfg(not(feature = "devices"))]
        let _ = listener;
        // audio had no bucket at all, so a game whose mixer
        // was the expensive thing could profile every frame and never see it.
        self.profile_record(floptle_core::profile::Bucket::Audio, audio_t.ms());
        // Last in the step, so a setting changed in an `update` takes effect
        // on the step it was changed on — a control that lags a frame behind
        // the click reads as one that did not work. `app.quit()` lands here
        // too, which is why it is after everything else this step wanted to
        // do rather than in the middle of it.
        self.apply_app_requests();
    }

    /// GPU-load models a script swapped via `node.model` so they render this
    /// frame.
    ///
    /// This is [`Editor::import_model`], once per changed node, and nothing
    /// else — so the path resolves against the project root, not the CWD. In
    /// the editor the two agree by accident (the CWD is the project dir); in
    /// an exported build the project ships as `assets/` and the CWD is
    /// wherever the player launched from, so a bare `Path::new` would miss
    /// every runtime model swap and the node would render as nothing, with
    /// only a stderr line no player sees.
    pub(crate) fn load_script_swapped_models(&mut self) {
        for (_eid, path) in self.script_host.take_model_changes() {
            self.import_model(&path);
        }
    }

    /// End-of-input bookkeeping: clear the per-frame key/button edges, re-pin a
    /// Confine-only cursor grab, and drain script logs into the Console.
    pub(crate) fn finish_input_frame(&mut self) {
        // Clear per-frame input edges after scripts consumed them.
        self.input_keys_pressed.clear();
        self.input_keys_released.clear();
        self.input_typed.clear();
        self.ui_text_ops.clear();
        self.input_buttons_pressed = [false; 3];
        self.input_mouse_delta = (0.0, 0.0);
        self.input_scroll = 0.0;
        // The per-tick accumulators are consumed by the gameplay-tick loop while
        // playing; outside play they'd grow unbounded, so drain them here instead.
        if !self.playing {
            self.tick_keys_pressed.clear();
            self.tick_keys_released.clear();
            self.tick_typed.clear();
            self.tick_buttons_pressed = [false; 3];
            self.tick_mouse_delta = (0.0, 0.0);
            self.tick_scroll = 0.0;
            // Same for the action layer's banked edges, and for the same
            // reason: nothing consumes them outside Play, so they would grow
            // without bound and then all fire at once on the first tick.
            self.tick_input_edges.0.clear();
            self.tick_input_edges.1.clear();
        }
        // A confine-only grab (X11 has no OS cursor lock) still lets the pointer
        // wander inside the window — pin it ourselves while a look/pan/lock/trap is
        // active. Look/pan read RAW device motion, so re-centering never pollutes
        // the deltas. A trapped Game cursor re-centers to the game rect (not the
        // window) so a Confined pointer stays inside the viewport it's playing in.
        if self.cursor_lock_soft
            && (self.game_holds_cursor() || self.input.looking || self.panning || self.game_trap)
            && let Some(window) = self.window.as_ref()
        {
            let sz = window.inner_size();
            let (cx, cy) = match self.game_surface_px() {
                Some((org, size)) if self.game_trap => {
                    ((org[0] + size[0] * 0.5) as u32, (org[1] + size[1] * 0.5) as u32)
                }
                _ => (sz.width / 2, sz.height / 2),
            };
            let _ = window.set_cursor_position(winit::dpi::PhysicalPosition::new(cx, cy));
        }
        // Hand the cursor back the moment the game puts something clickable on
        // screen — asked every frame, not only at the click that trapped.
        self.release_trap_for_ui();
        // Safety: never stay trapped once play stops (e.g. Stop while trapped, or a
        // layout change hid the Game tab). Escape/Stop already handle the common path.
        if self.game_trap && !self.playing {
            self.game_trap = false;
            if let Some(window) = self.window.as_ref() {
                self.cursor_lock_soft = grab_cursor(window, false);
            }
        }
        // Same safety for the editor's pointer override: it only means anything
        // against a running game, and a stale one would eat the first lock the
        // next session asks for.
        if self.cursor_freed && !self.playing {
            self.cursor_freed = false;
        }
        self.drain_script_logs();
    }

    /// Move whatever the scripts said this frame into the Console.
    ///
    /// Its own function because there are **three** loops that have to do it
    /// and only one of them is a frame: `finish_input_frame` for the editor,
    /// `floptle run` for a headless one, and the dedicated server. When this
    /// lived inline in the frame, a headless run collected nothing at all and
    /// reported "nothing raised" for a project whose script was raising every
    /// step — the worst answer available, because it is confident and wrong.
    pub(crate) fn drain_script_logs(&mut self) {
        self.adopt_script_logs(true);
    }

    /// [`Self::drain_script_logs`] without the terminal echo, for a host that
    /// prints the Console itself.
    ///
    /// The echo exists because running the editor from a terminal should not
    /// mean opening the Console panel to see `log(...)` — but a dedicated
    /// server's whole log is the Console, drained to stderr every tick, and
    /// echoing here as well would print every line a script writes twice.
    pub(crate) fn adopt_script_logs(&mut self, echo: bool) {
        // On **stderr**: stdout belongs to whatever the caller asked for, and
        // a verb's `--json` document is on it. Locked once for the drain
        // rather than per line, and a failed write is dropped: this is the
        // hottest print in the process — a thousand lines a frame at the
        // cap — and the descriptor it writes to is whatever launched the
        // editor, which may have gone away.
        use std::io::Write as _;
        let mut err = echo.then(|| std::io::stderr().lock());
        // An asset reference that tried to leave the project, said once.
        for msg in crate::project::take_refused_refs() {
            if let Some(err) = err.as_mut() {
                let _ = writeln!(err, "[assets] {msg}");
            }
            self.console.push(floptle_script::LogLevel::Warn, msg, None);
        }
        for l in self.script_host.drain_logs() {
            if let Some(err) = err.as_mut() {
                let _ = writeln!(err, "[lua] {}", l.msg);
            }
            self.console.push(l.level, l.msg, l.source);
        }
    }
}
