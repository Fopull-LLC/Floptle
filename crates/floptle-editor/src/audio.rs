//! Editor-side audio glue: the clip cache, play-mode voices for `AudioSource`
//! components, script one-shots (`audio.play`), and the runtime mixer overlay.
//!
//! The pure engine lives in `floptle-audio`; this module connects it to the
//! live editor world — the same layering as [`crate::vfx`]. One field on
//! `Editor`; works identically in editor Play and exported (`--play`) builds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use floptle_audio::{
    AudioEngine, AudioSource, ClipRef, EndBehavior, Listener, MixerDesc, PlayParams, SpatialMode,
    VoiceId,
};
use floptle_core::math::DVec3;
use floptle_core::{Entity, World};
use floptle_script::{AudioAt, AudioCmd, AudioInfo, AudioPlayState};

/// A script one-shot (`audio.play`), keyed by its script-side handle.
struct ScriptSound {
    voice: VoiceId,
    /// Node this sound follows (`audio.play(clip, node, …)`).
    follow: Option<Entity>,
    /// Live copy of the voice's params (updated by `sound:setVolume` etc.).
    params: PlayParams,
}

/// Everything audio the editor owns. One field on `Editor`.
#[derive(Default)]
pub struct AudioSystem {
    engine: Option<AudioEngine>,
    /// The device failed to open (headless / no output) — don't retry every frame.
    engine_failed: bool,
    /// Decoded clips by project-relative path; `None` = load failed (logged once).
    clips: HashMap<String, Option<ClipRef>>,
    /// Live play-mode voices per `AudioSource` entity.
    source_voices: HashMap<Entity, VoiceId>,
    /// Last-synced component per entity, for change detection during Play.
    source_cache: HashMap<Entity, AudioSource>,
    /// Script one-shots by script handle.
    sounds: HashMap<u32, ScriptSound>,
    /// Asset-browser preview voice (outside Play).
    preview: Option<VoiceId>,
    /// The play session's live mixer (project mixer + Lua tweaks); reverts on Stop.
    pub runtime_mixer: Option<MixerDesc>,
}

impl AudioSystem {
    /// The engine handle, opening the output device on first use. `None` on
    /// machines with no audio output — every caller degrades to silence.
    pub fn engine(&mut self) -> Option<&mut AudioEngine> {
        if self.engine.is_none() && !self.engine_failed {
            match AudioEngine::new() {
                Ok(e) => self.engine = Some(e),
                Err(err) => {
                    log::warn!("audio disabled: {err}");
                    self.engine_failed = true;
                }
            }
        }
        self.engine.as_mut()
    }

    /// Resolve a clip reference to a file: the project-relative path as
    /// written, else with each audio extension appended (so Lua can say
    /// `"audio/hit"`).
    fn resolve_clip_path(root: &Path, key: &str) -> Option<PathBuf> {
        let direct = root.join(key);
        if direct.is_file() {
            return Some(direct);
        }
        for ext in floptle_audio::AUDIO_EXTENSIONS {
            let p = root.join(format!("{key}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }

    /// Load (or fetch cached) a clip by project-relative key.
    pub fn clip(&mut self, root: &Path, key: &str) -> Option<ClipRef> {
        if key.is_empty() {
            return None;
        }
        if let Some(cached) = self.clips.get(key) {
            return cached.clone();
        }
        let loaded = match Self::resolve_clip_path(root, key) {
            Some(path) => match floptle_audio::load_clip(&path) {
                Ok(c) => Some(std::sync::Arc::new(c)),
                Err(e) => {
                    log::warn!("audio: {e}");
                    None
                }
            },
            None => {
                log::warn!("audio: clip not found: {key}");
                None
            }
        };
        self.clips.insert(key.to_string(), loaded.clone());
        loaded
    }

    /// Push a mixer graph to the engine (editor preview + play mode).
    pub fn apply_mixer(&mut self, desc: &MixerDesc) {
        if let Some(eng) = self.engine() {
            eng.set_mixer(desc);
        }
    }

    /// World position of an entity (its transform composed through parents).
    fn world_pos(world: &World, e: Entity) -> DVec3 {
        floptle_core::world_transform(world, e).translation
    }

    /// Play (or restart) the voice for an `AudioSource` component.
    fn source_play(&mut self, world: &World, root: &Path, e: Entity) {
        let Some(src) = world.get::<AudioSource>(e).cloned() else { return };
        let Some(clip) = self.clip(root, &src.clip) else { return };
        let pos = Self::world_pos(world, e);
        if let Some(old) = self.source_voices.remove(&e)
            && let Some(eng) = self.engine()
        {
            eng.stop(old);
        }
        if let Some(eng) = self.engine() {
            let id = eng.play(clip, Some(pos), src.params.clone());
            self.source_voices.insert(e, id);
            self.source_cache.insert(e, src);
        }
    }

    /// Play-mode start: apply the project mixer and fire play-on-start sources.
    pub fn start_play(&mut self, world: &World, root: &Path, mixer: &MixerDesc) {
        self.stop_preview();
        self.runtime_mixer = Some(mixer.clone());
        self.apply_mixer(mixer);
        let starters: Vec<Entity> = world
            .query::<AudioSource>()
            .filter(|(_, s)| s.play_on_start && !s.clip.is_empty())
            .map(|(e, _)| e)
            .collect();
        for e in starters {
            self.source_play(world, root, e);
        }
    }

    /// Play-mode stop: silence everything, clear session state, restore the
    /// saved project mixer for editor-side previews.
    pub fn stop_play(&mut self, project_mixer: &MixerDesc) {
        if let Some(eng) = self.engine() {
            eng.reset();
            eng.drain_finished();
        }
        self.source_voices.clear();
        self.source_cache.clear();
        self.sounds.clear();
        self.runtime_mixer = None;
        self.apply_mixer(project_mixer);
    }

    /// Per-frame play-mode tick: listener, source sync, follow updates,
    /// finished-voice reaping. Returns nodes to despawn (`EndBehavior::Destroy`).
    pub fn advance(&mut self, world: &World, root: &Path, listener: Listener) -> Vec<Entity> {
        if self.engine.is_none() {
            return Vec::new();
        }
        if let Some(eng) = self.engine() {
            eng.set_listener(listener);
        }

        // ---- sync AudioSource components -> voices --------------------------
        let live: Vec<(Entity, AudioSource, DVec3)> = world
            .query::<AudioSource>()
            .map(|(e, s)| (e, s.clone(), Self::world_pos(world, e)))
            .collect();
        for (e, src, pos) in &live {
            let Some(&voice) = self.source_voices.get(e) else { continue };
            match self.source_cache.get(e) {
                Some(prev) if prev.clip != src.clip => {
                    // Clip swapped mid-play: restart on the new clip.
                    self.source_play(world, root, *e);
                    continue;
                }
                Some(prev) if prev.params != src.params => {
                    if let Some(eng) = self.engine() {
                        eng.update_params(voice, src.params.clone());
                    }
                    self.source_cache.insert(*e, src.clone());
                }
                _ => {}
            }
            if let Some(eng) = self.engine() {
                eng.move_voice(voice, *pos);
            }
        }
        // Sources whose entity vanished (node deleted mid-play).
        let gone: Vec<Entity> =
            self.source_voices.keys().filter(|e| world.get::<AudioSource>(**e).is_none()).copied().collect();
        for e in gone {
            if let Some(v) = self.source_voices.remove(&e)
                && let Some(eng) = self.engine()
            {
                eng.stop(v);
            }
            self.source_cache.remove(&e);
        }

        // ---- follow script one-shots ----------------------------------------
        let mut orphaned: Vec<u32> = Vec::new();
        let moves: Vec<(VoiceId, Option<DVec3>, u32)> = self
            .sounds
            .iter()
            .filter_map(|(h, s)| {
                let f = s.follow?;
                if world.get::<floptle_core::Transform>(f).is_some() {
                    Some((s.voice, Some(Self::world_pos(world, f)), *h))
                } else {
                    Some((s.voice, None, *h))
                }
            })
            .collect();
        for (voice, pos, handle) in moves {
            match pos {
                Some(p) => {
                    if let Some(eng) = self.engine() {
                        eng.move_voice(voice, p);
                    }
                }
                None => {
                    // Followed node despawned: fade the sound out too.
                    if let Some(eng) = self.engine() {
                        eng.stop(voice);
                    }
                    orphaned.push(handle);
                }
            }
        }
        for h in orphaned {
            if let Some(s) = self.sounds.get_mut(&h) {
                s.follow = None;
            }
        }

        // ---- reap finished voices --------------------------------------------
        let finished = self.engine().map(|e| e.drain_finished()).unwrap_or_default();
        let mut despawn = Vec::new();
        for id in finished {
            if let Some((&e, _)) = self.source_voices.iter().find(|(_, v)| **v == id) {
                let destroy = self
                    .source_cache
                    .get(&e)
                    .is_some_and(|s| s.params.end == EndBehavior::Destroy);
                self.source_voices.remove(&e);
                self.source_cache.remove(&e);
                if destroy {
                    despawn.push(e);
                }
            }
            if let Some((&h, _)) = self.sounds.iter().find(|(_, s)| s.voice == id) {
                let s = self.sounds.remove(&h).expect("just found");
                if s.params.end == EndBehavior::Destroy
                    && let Some(f) = s.follow
                {
                    despawn.push(f);
                }
            }
            if self.preview == Some(id) {
                self.preview = None;
            }
        }
        despawn
    }

    /// Apply the audio commands scripts queued this frame. `runtime` is the
    /// play session's live mixer desc (Lua track tweaks land there).
    pub fn apply_script_commands(&mut self, world: &World, root: &Path, cmds: Vec<AudioCmd>) {
        let mut mixer_dirty = false;
        for cmd in cmds {
            match cmd {
                AudioCmd::Play { handle, clip, at, params } => {
                    let Some(clip) = self.clip(root, &clip) else { continue };
                    let mut params = *params;
                    let (emitter, follow) = match at {
                        AudioAt::Flat => {
                            params.mode = SpatialMode::Flat;
                            (None, None)
                        }
                        AudioAt::Pos(p) => (Some(DVec3::from_array(p)), None),
                        AudioAt::Node(idx) => {
                            match world
                                .query::<floptle_core::Transform>()
                                .find(|(e, _)| e.index() == idx)
                                .map(|(e, _)| e)
                            {
                                Some(e) => (Some(Self::world_pos(world, e)), Some(e)),
                                None => (None, None),
                            }
                        }
                    };
                    if let Some(eng) = self.engine() {
                        let voice = eng.play(clip, emitter, params.clone());
                        self.sounds.insert(handle, ScriptSound { voice, follow, params });
                    }
                }
                AudioCmd::Stop { handle } => {
                    if let Some(s) = self.sounds.get(&handle) {
                        let v = s.voice;
                        if let Some(eng) = self.engine() {
                            eng.stop(v);
                        }
                    }
                }
                AudioCmd::Pause { handle, paused } => {
                    if let Some(s) = self.sounds.get(&handle) {
                        let v = s.voice;
                        if let Some(eng) = self.engine() {
                            eng.set_paused(v, paused);
                        }
                    }
                }
                AudioCmd::SetParam { handle, field, value } => {
                    if let Some(s) = self.sounds.get_mut(&handle) {
                        match field.as_str() {
                            "volume" => s.params.volume = (value as f32).clamp(0.0, 4.0),
                            "pitch" => s.params.pitch = (value as f32).clamp(0.05, 8.0),
                            "pan" => s.params.pan = (value as f32).clamp(-1.0, 1.0),
                            _ => {}
                        }
                        let (v, p) = (s.voice, s.params.clone());
                        if let Some(eng) = self.engine() {
                            eng.update_params(v, p);
                        }
                    }
                }
                AudioCmd::SetTrack { handle, track } => {
                    if let Some(s) = self.sounds.get_mut(&handle) {
                        s.params.track = track;
                        let (v, p) = (s.voice, s.params.clone());
                        if let Some(eng) = self.engine() {
                            eng.update_params(v, p);
                        }
                    }
                }
                AudioCmd::Move { handle, pos } => {
                    if let Some(s) = self.sounds.get_mut(&handle) {
                        s.follow = None; // manual placement overrides following
                        let v = s.voice;
                        if let Some(eng) = self.engine() {
                            eng.move_voice(v, DVec3::from_array(pos));
                        }
                    }
                }
                AudioCmd::Seek { handle, secs } => {
                    if let Some(s) = self.sounds.get(&handle) {
                        let v = s.voice;
                        if let Some(eng) = self.engine() {
                            eng.seek(v, secs as f32);
                        }
                    }
                }
                AudioCmd::StopAll => {
                    if let Some(eng) = self.engine() {
                        eng.stop_all();
                    }
                }
                AudioCmd::SourcePlay { ent } => {
                    if let Some(e) = Self::source_entity(world, ent) {
                        self.source_play(world, root, e);
                    }
                }
                AudioCmd::SourceStop { ent } => {
                    if let Some(e) = Self::source_entity(world, ent)
                        && let Some(v) = self.source_voices.remove(&e)
                    {
                        self.source_cache.remove(&e);
                        if let Some(eng) = self.engine() {
                            eng.stop(v);
                        }
                    }
                }
                AudioCmd::SourcePause { ent, paused } => {
                    if let Some(e) = Self::source_entity(world, ent)
                        && let Some(&v) = self.source_voices.get(&e)
                        && let Some(eng) = self.engine()
                    {
                        eng.set_paused(v, paused);
                    }
                }
                AudioCmd::SourceSetClip { .. } => {
                    // The clip string lands on the component via the flush in
                    // render_frame (it mutates World state) — handled there.
                }
                AudioCmd::SourceSeek { ent, secs } => {
                    if let Some(e) = Self::source_entity(world, ent)
                        && let Some(&v) = self.source_voices.get(&e)
                        && let Some(eng) = self.engine()
                    {
                        eng.seek(v, secs as f32);
                    }
                }
                AudioCmd::TrackVolume { track, db } => {
                    if let Some(t) = self.runtime_track(&track) {
                        t.gain_db = (db as f32).clamp(-80.0, 24.0);
                        mixer_dirty = true;
                    }
                }
                AudioCmd::TrackPan { track, pan } => {
                    if let Some(t) = self.runtime_track(&track) {
                        t.pan = (pan as f32).clamp(-1.0, 1.0);
                        mixer_dirty = true;
                    }
                }
                AudioCmd::TrackMuted { track, muted } => {
                    if let Some(t) = self.runtime_track(&track) {
                        t.muted = muted;
                        mixer_dirty = true;
                    }
                }
                AudioCmd::TrackSoloed { track, soloed } => {
                    if let Some(t) = self.runtime_track(&track) {
                        t.soloed = soloed;
                        mixer_dirty = true;
                    }
                }
            }
        }
        if mixer_dirty && let Some(m) = self.runtime_mixer.clone() {
            self.apply_mixer(&m);
        }
    }

    /// Resolve an entity index from a script command to a live AudioSource node.
    fn source_entity(world: &World, idx: u32) -> Option<Entity> {
        world.query::<AudioSource>().find(|(e, _)| e.index() == idx).map(|(e, _)| e)
    }

    /// The play session's live copy of a mixer track ("Master" = the master).
    fn runtime_track(&mut self, name: &str) -> Option<&mut floptle_audio::TrackDesc> {
        let m = self.runtime_mixer.as_mut()?;
        if name == floptle_audio::MASTER {
            return Some(&mut m.master);
        }
        m.track_mut(name)
    }

    /// Build the playback mirror scripts read (`sound:isPlaying()` etc.).
    pub fn script_info(&mut self) -> AudioInfo {
        let mut info = AudioInfo::default();
        let sounds: Vec<(u32, VoiceId)> = self.sounds.iter().map(|(h, s)| (*h, s.voice)).collect();
        let sources: Vec<(u32, VoiceId)> =
            self.source_voices.iter().map(|(e, v)| (e.index(), *v)).collect();
        if let Some(eng) = self.engine() {
            for (h, v) in sounds {
                if let Some(st) = eng.status(v) {
                    info.sounds.insert(
                        h,
                        AudioPlayState {
                            playing: st.playing,
                            paused: st.paused,
                            position: st.position_secs as f64,
                        },
                    );
                }
            }
            for (idx, v) in sources {
                if let Some(st) = eng.status(v) {
                    info.sources.insert(
                        idx,
                        AudioPlayState {
                            playing: st.playing,
                            paused: st.paused,
                            position: st.position_secs as f64,
                        },
                    );
                }
            }
        }
        info
    }

    /// Asset-browser preview: play a clip flat, replacing any prior preview.
    pub fn preview(&mut self, root: &Path, key: &str) {
        self.stop_preview();
        let Some(clip) = self.clip(root, key) else { return };
        if let Some(eng) = self.engine() {
            let params =
                PlayParams { mode: SpatialMode::Flat, ..Default::default() };
            self.preview = Some(eng.play(clip, None, params));
        }
    }

    pub fn stop_preview(&mut self) {
        if let Some(v) = self.preview.take()
            && let Some(eng) = self.engine()
        {
            eng.stop(v);
        }
    }

    /// Post-fader track meters (master first) for the Mixer tab.
    pub fn meters(&mut self) -> Vec<(String, f32)> {
        self.engine().map(|e| e.meters()).unwrap_or_default()
    }
}
