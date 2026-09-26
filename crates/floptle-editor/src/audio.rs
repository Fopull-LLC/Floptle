//! Editor-side audio glue: the clip cache, play-mode voices for `AudioSource`
//! components, script one-shots (`audio.play`), and the runtime mixer overlay.
//!
//! The pure engine lives in `floptle-audio`; this module connects it to the
//! live editor world — the same layering as [`crate::vfx`]. One field on
//! `Editor`; works identically in editor Play and exported (`--play`) builds.
//!
//! A clip is decoded off the frame ([`crate::audio_clips`]), so a sound asked
//! for before its clip is in is held as [`Waiting`] and started by
//! [`AudioSystem::pump`] once it is. Until then it reads as playing, from
//! position zero, with `sound:isLoading()` true.

use std::collections::HashMap;
use std::path::Path;

use floptle_audio::{
    AudioEngine, AudioSource, ClipRef, EndBehavior, Listener, MixerDesc, PlayParams, SpatialMode,
    VoiceId,
};
use floptle_core::math::DVec3;
use floptle_core::{Entity, World};
use floptle_script::{AudioAt, AudioCmd, AudioInfo, AudioPlayState};

use crate::audio_clips::{ClipCache, ClipState};

/// A sound that has been asked for and whose clip is still decoding. What a
/// script does to it in the meantime is kept here and applied when it
/// starts.
#[derive(Clone, Debug, Default)]
struct Waiting {
    clip: String,
    /// Where it plays, for a sound placed at a point (`audio.play(clip, x, y, z)`).
    emitter: Option<DVec3>,
    paused: bool,
    seek: Option<f32>,
}

impl Waiting {
    fn state(&self) -> AudioPlayState {
        AudioPlayState {
            playing: true,
            paused: self.paused,
            position: self.seek.unwrap_or(0.0) as f64,
            loading: true,
        }
    }
}

/// A script one-shot (`audio.play`), keyed by its script-side handle.
struct ScriptSound {
    /// `Ok` once it is mixing; `Err` while its clip decodes.
    voice: Result<VoiceId, Waiting>,
    /// Node this sound follows (`audio.play(clip, node, …)`).
    follow: Option<Entity>,
    /// Live copy of the voice's params (updated by `sound:setVolume` etc.).
    params: PlayParams,
}

impl ScriptSound {
    fn live(&self) -> Option<VoiceId> {
        self.voice.as_ref().ok().copied()
    }
}

/// Everything audio the editor owns. One field on `Editor`.
#[derive(Default)]
pub struct AudioSystem {
    engine: Option<AudioEngine>,
    /// The device failed to open (headless / no output) — don't retry every frame.
    engine_failed: bool,
    /// Decoded clips by project-relative path.
    clips: ClipCache,
    /// Live play-mode voices per `AudioSource` entity.
    source_voices: HashMap<Entity, VoiceId>,
    /// `AudioSource`s told to play whose clip is still decoding.
    source_waiting: HashMap<Entity, Waiting>,
    /// Last-synced component per entity, for change detection during Play.
    source_cache: HashMap<Entity, AudioSource>,
    /// Script one-shots by script handle.
    sounds: HashMap<u32, ScriptSound>,
    /// Asset-browser preview voice (outside Play).
    preview: Option<VoiceId>,
    /// The file an audition is waiting on.
    preview_waiting: Option<String>,
    /// The play session's live mixer (project mixer + Lua tweaks); reverts on Stop.
    pub runtime_mixer: Option<MixerDesc>,
}

impl AudioSystem {
    /// Voices this frame is mixing: every `AudioSource` node playing, plus every
    /// script one-shot still running.
    ///
    /// The preview voice is excluded — it only exists outside Play,
    /// and a number a game reads to check its own budget should not count the
    /// editor auditioning a file. A sound still waiting for its clip is not
    /// mixing yet, so it is not counted either.
    pub fn live_voices(&self) -> usize {
        self.source_voices.len() + self.sounds.values().filter(|s| s.live().is_some()).count()
    }

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

    /// Start decoding clips a script will want soon (`audio.preload`), and
    /// keep them until their first play. Nothing is decoded on a host with
    /// no output, where nothing would ever play them.
    pub fn preload(&mut self, root: &Path, key: &str) {
        if self.engine().is_some() {
            self.clips.preload(root, key);
        }
    }

    /// A preloaded clip: `Some(true)` in, `Some(false)` failed, `None` still
    /// decoding. A host with no output answers "in" — there is nothing to wait
    /// for, and a game waiting on a preload must run there the same as
    /// anywhere.
    pub fn clip_status(&mut self, key: &str) -> Option<bool> {
        if self.engine().is_none() { Some(true) } else { self.clips.status(key) }
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

    /// Play (or restart) the voice for an `AudioSource` component — now if its
    /// clip is in, else as soon as it is.
    fn source_play(&mut self, world: &World, root: &Path, e: Entity) {
        let Some(src) = world.get::<AudioSource>(e) else { return };
        if src.clip.is_empty() || self.engine().is_none() {
            return;
        }
        let key = src.clip.clone();
        if let Some(old) = self.source_voices.remove(&e)
            && let Some(eng) = self.engine()
        {
            eng.stop(old);
        }
        self.source_cache.remove(&e);
        match self.clips.get(root, &key) {
            ClipState::Ready(clip) => {
                self.source_waiting.remove(&e);
                self.source_start(world, e, clip, Waiting::default());
            }
            ClipState::Loading => {
                self.source_waiting.insert(e, Waiting { clip: key, ..Default::default() });
            }
            ClipState::Failed => {
                self.source_waiting.remove(&e);
            }
        }
    }

    fn source_start(&mut self, world: &World, e: Entity, clip: ClipRef, w: Waiting) {
        let Some(src) = world.get::<AudioSource>(e).cloned() else { return };
        let pos = Self::world_pos(world, e);
        let Some(eng) = self.engine() else { return };
        let id = eng.play(clip, Some(pos), src.params.clone());
        if let Some(secs) = w.seek {
            eng.seek(id, secs);
        }
        if w.paused {
            eng.set_paused(id, true);
        }
        self.source_voices.insert(e, id);
        self.source_cache.insert(e, src);
    }

    /// Start every sound whose clip has come in since the last frame, let go
    /// of ones whose clip could not be decoded, and trim idle clips. Once a
    /// frame, in Play and out of it (an audition waits here too).
    pub fn pump(&mut self, world: &World, root: &Path) {
        let waiting = !self.source_waiting.is_empty()
            || self.preview_waiting.is_some()
            || self.sounds.values().any(|s| s.voice.is_err());
        if waiting || self.clips.is_loading() {
            self.clips.receive();
            self.start_arrived(world, root);
        }
        // After the starts: a clip that just came in is held by its voice now,
        // and one bigger than the whole budget is not dropped on arrival.
        self.clips.evict();
    }

    fn start_arrived(&mut self, world: &World, root: &Path) {
        // ---- script one-shots ------------------------------------------------
        let handles: Vec<u32> =
            self.sounds.iter().filter(|(_, s)| s.voice.is_err()).map(|(h, _)| *h).collect();
        for h in handles {
            let Some(Err(w)) = self.sounds.get(&h).map(|s| s.voice.clone()) else { continue };
            match self.clips.peek(&w.clip) {
                ClipState::Loading => {}
                ClipState::Failed => {
                    self.sounds.remove(&h);
                }
                ClipState::Ready(clip) => {
                    let (follow, params) = {
                        let s = &self.sounds[&h];
                        (s.follow, s.params.clone())
                    };
                    let emitter = match follow {
                        Some(f) => Some(Self::world_pos(world, f)),
                        None => w.emitter,
                    };
                    let Some(eng) = self.engine() else { continue };
                    let id = eng.play(clip, emitter, params);
                    if let Some(secs) = w.seek {
                        eng.seek(id, secs);
                    }
                    if w.paused {
                        eng.set_paused(id, true);
                    }
                    if let Some(s) = self.sounds.get_mut(&h) {
                        s.voice = Ok(id);
                    }
                }
            }
        }

        // ---- AudioSource components ------------------------------------------
        let ents: Vec<Entity> = self.source_waiting.keys().copied().collect();
        for e in ents {
            let Some(src) = world.get::<AudioSource>(e) else {
                self.source_waiting.remove(&e);
                continue;
            };
            let w = self.source_waiting[&e].clone();
            if src.clip != w.clip {
                // Given another clip while this one decoded: wait for that one.
                let key = src.clip.clone();
                self.source_waiting.remove(&e);
                if !key.is_empty() {
                    match self.clips.get(root, &key) {
                        ClipState::Ready(clip) => self.source_start(world, e, clip, w),
                        ClipState::Loading => {
                            self.source_waiting.insert(e, Waiting { clip: key, ..w });
                        }
                        ClipState::Failed => {}
                    }
                }
                continue;
            }
            match self.clips.peek(&w.clip) {
                ClipState::Loading => {}
                ClipState::Failed => {
                    self.source_waiting.remove(&e);
                }
                ClipState::Ready(clip) => {
                    self.source_waiting.remove(&e);
                    self.source_start(world, e, clip, w);
                }
            }
        }

        // ---- the Assets browser's audition -----------------------------------
        if let Some(key) = self.preview_waiting.clone() {
            match self.clips.peek(&key) {
                ClipState::Loading => {}
                ClipState::Failed => self.preview_waiting = None,
                ClipState::Ready(clip) => {
                    self.preview_waiting = None;
                    self.preview_start(clip);
                }
            }
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
    /// saved project mixer for editor-side previews. A clip still decoding
    /// carries on and lands in the cache; nothing waits on it any more.
    pub fn stop_play(&mut self, project_mixer: &MixerDesc) {
        if let Some(eng) = self.engine() {
            eng.reset();
            eng.drain_finished();
        }
        self.source_voices.clear();
        self.source_waiting.clear();
        self.source_cache.clear();
        self.sounds.clear();
        self.runtime_mixer = None;
        self.apply_mixer(project_mixer);
    }

    /// Per-frame play-mode tick: clips that came in, listener, source sync,
    /// follow updates, finished-voice reaping. Returns nodes to despawn
    /// (`EndBehavior::Destroy`).
    pub fn advance(&mut self, world: &World, root: &Path, listener: Listener) -> Vec<Entity> {
        if self.engine.is_none() {
            return Vec::new();
        }
        self.pump(world, root);
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
        let moves: Vec<(Option<VoiceId>, Option<DVec3>, u32)> = self
            .sounds
            .iter()
            .filter_map(|(h, s)| {
                let f = s.follow?;
                if world.get::<floptle_core::Transform>(f).is_some() {
                    Some((s.live(), Some(Self::world_pos(world, f)), *h))
                } else {
                    Some((s.live(), None, *h))
                }
            })
            .collect();
        for (voice, pos, handle) in moves {
            match (voice, pos) {
                (Some(voice), Some(p)) => {
                    if let Some(eng) = self.engine() {
                        eng.move_voice(voice, p);
                    }
                }
                // Still waiting: it picks the node's position up when it starts.
                (None, Some(_)) => {}
                (Some(voice), None) => {
                    // Followed node despawned: fade the sound out too.
                    if let Some(eng) = self.engine() {
                        eng.stop(voice);
                    }
                    orphaned.push(handle);
                }
                // Its node went before it ever started: it never will.
                (None, None) => {
                    self.sounds.remove(&handle);
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
            if let Some((&h, _)) = self.sounds.iter().find(|(_, s)| s.live() == Some(id)) {
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
                    // Checked before the clip: a host with no output (a
                    // dedicated server) has no reason to decode a thing.
                    if self.engine().is_none() {
                        continue;
                    }
                    let mut params = *params;
                    let (emitter, follow) = match at {
                        AudioAt::Flat => {
                            params.mode = SpatialMode::Flat;
                            (None, None)
                        }
                        AudioAt::Pos(p) => (Some(DVec3::from_array(p)), None),
                        AudioAt::Node(idx) => {
                            match world.entity_with::<floptle_core::Transform>(idx) {
                                Some(e) => (Some(Self::world_pos(world, e)), Some(e)),
                                None => (None, None),
                            }
                        }
                    };
                    let voice = match self.clips.get(root, &clip) {
                        ClipState::Failed => continue,
                        ClipState::Loading => Err(Waiting { clip, emitter, ..Default::default() }),
                        ClipState::Ready(clip) => match self.engine() {
                            Some(eng) => Ok(eng.play(clip, emitter, params.clone())),
                            None => continue,
                        },
                    };
                    self.sounds.insert(handle, ScriptSound { voice, follow, params });
                }
                AudioCmd::Stop { handle } => match self.sounds.get(&handle).map(|s| s.live()) {
                    Some(Some(v)) => {
                        if let Some(eng) = self.engine() {
                            eng.stop(v);
                        }
                    }
                    Some(None) => {
                        self.sounds.remove(&handle);
                    }
                    None => {}
                },
                AudioCmd::Pause { handle, paused } => {
                    let Some(s) = self.sounds.get_mut(&handle) else { continue };
                    match &mut s.voice {
                        Ok(v) => {
                            let v = *v;
                            if let Some(eng) = self.engine() {
                                eng.set_paused(v, paused);
                            }
                        }
                        Err(w) => w.paused = paused,
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
                        let (v, p) = (s.live(), s.params.clone());
                        if let Some(v) = v
                            && let Some(eng) = self.engine()
                        {
                            eng.update_params(v, p);
                        }
                    }
                }
                AudioCmd::SetTrack { handle, track } => {
                    if let Some(s) = self.sounds.get_mut(&handle) {
                        s.params.track = track;
                        let (v, p) = (s.live(), s.params.clone());
                        if let Some(v) = v
                            && let Some(eng) = self.engine()
                        {
                            eng.update_params(v, p);
                        }
                    }
                }
                AudioCmd::Move { handle, pos } => {
                    let Some(s) = self.sounds.get_mut(&handle) else { continue };
                    s.follow = None; // manual placement overrides following
                    let pos = DVec3::from_array(pos);
                    match &mut s.voice {
                        Ok(v) => {
                            let v = *v;
                            if let Some(eng) = self.engine() {
                                eng.move_voice(v, pos);
                            }
                        }
                        Err(w) => w.emitter = Some(pos),
                    }
                }
                AudioCmd::Seek { handle, secs } => {
                    let Some(s) = self.sounds.get_mut(&handle) else { continue };
                    match &mut s.voice {
                        Ok(v) => {
                            let v = *v;
                            if let Some(eng) = self.engine() {
                                eng.seek(v, secs as f32);
                            }
                        }
                        Err(w) => w.seek = Some(secs as f32),
                    }
                }
                AudioCmd::StopAll => {
                    if let Some(eng) = self.engine() {
                        eng.stop_all();
                    }
                    self.sounds.retain(|_, s| s.voice.is_ok());
                    self.source_waiting.clear();
                }
                AudioCmd::SourcePlay { ent } => {
                    if let Some(e) = Self::source_entity(world, ent) {
                        self.source_play(world, root, e);
                    }
                }
                AudioCmd::SourceStop { ent } => {
                    if let Some(e) = Self::source_entity(world, ent) {
                        self.source_waiting.remove(&e);
                        if let Some(v) = self.source_voices.remove(&e) {
                            self.source_cache.remove(&e);
                            if let Some(eng) = self.engine() {
                                eng.stop(v);
                            }
                        }
                    }
                }
                AudioCmd::SourcePause { ent, paused } => {
                    let Some(e) = Self::source_entity(world, ent) else { continue };
                    if let Some(w) = self.source_waiting.get_mut(&e) {
                        w.paused = paused;
                    } else if let Some(&v) = self.source_voices.get(&e)
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
                    let Some(e) = Self::source_entity(world, ent) else { continue };
                    if let Some(w) = self.source_waiting.get_mut(&e) {
                        w.seek = Some(secs as f32);
                    } else if let Some(&v) = self.source_voices.get(&e)
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
        world.entity_with::<AudioSource>(idx)
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
        let mut sounds: Vec<(u32, VoiceId)> = Vec::new();
        for (h, s) in &self.sounds {
            match &s.voice {
                Ok(v) => sounds.push((*h, *v)),
                Err(w) => {
                    info.sounds.insert(*h, w.state());
                }
            }
        }
        for (e, w) in &self.source_waiting {
            info.sources.insert(e.index(), w.state());
        }
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
                            loading: false,
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
                            loading: false,
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
        if self.engine().is_none() {
            return;
        }
        match self.clips.get(root, key) {
            ClipState::Ready(clip) => self.preview_start(clip),
            ClipState::Loading => self.preview_waiting = Some(key.to_string()),
            ClipState::Failed => {}
        }
    }

    fn preview_start(&mut self, clip: ClipRef) {
        if let Some(eng) = self.engine() {
            let params =
                PlayParams { mode: SpatialMode::Flat, ..Default::default() };
            self.preview = Some(eng.play(clip, None, params));
        }
    }

    pub fn stop_preview(&mut self) {
        self.preview_waiting = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_clips::tests::{fixed, gated, project};
    use crate::audio_clips::IDLE_BUDGET_BYTES;
    use std::time::{Duration, Instant};

    fn system(decode: crate::audio_clips::Decoder) -> AudioSystem {
        AudioSystem {
            engine: Some(AudioEngine::silent()),
            clips: ClipCache::with_decoder(decode, IDLE_BUDGET_BYTES),
            ..Default::default()
        }
    }

    fn play(handle: u32, clip: &str) -> AudioCmd {
        AudioCmd::Play { handle, clip: clip.into(), at: AudioAt::Flat, params: Box::default() }
    }

    /// Pump until the sound has a voice, or give up.
    fn until_live(a: &mut AudioSystem, world: &World, root: &Path, handle: u32) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            a.pump(world, root);
            if a.sounds.get(&handle).is_some_and(|s| s.live().is_some()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the sound never started");
    }

    /// A playlist polls `isPlaying()` to know when a track has ended. A track
    /// that read as stopped while its file decoded would be skipped at once.
    #[test]
    fn a_sound_waiting_for_its_clip_reads_as_playing_and_keeps_what_it_was_told() {
        let root = project("waiting", &["track.mp3"]);
        let world = World::default();
        let (decode, open, _) = gated(48_000);
        let mut a = system(decode);

        a.apply_script_commands(&world, &root, vec![play(1, "audio/track.mp3")]);
        let st = a.script_info().sounds[&1];
        assert!(st.playing && st.loading, "a sound waiting for its clip read as {st:?}");
        assert_eq!(a.live_voices(), 0, "a waiting sound is not mixing");

        a.apply_script_commands(
            &world,
            &root,
            vec![AudioCmd::Seek { handle: 1, secs: 0.5 }, AudioCmd::Pause { handle: 1, paused: true }],
        );
        let st = a.script_info().sounds[&1];
        assert!(st.paused && (st.position - 0.5).abs() < 1e-6, "lost what it was told: {st:?}");

        open.send(()).unwrap();
        until_live(&mut a, &world, &root, 1);
        let st = a.script_info().sounds[&1];
        assert!(st.playing && !st.loading, "started, and still says loading: {st:?}");
        assert_eq!(a.live_voices(), 1);
    }

    #[test]
    fn a_sound_stopped_before_its_clip_arrives_never_starts() {
        let root = project("stopped", &["track.mp3", "other.mp3"]);
        let world = World::default();
        let (decode, open, _) = gated(48_000);
        let mut a = system(decode);

        a.apply_script_commands(&world, &root, vec![play(1, "audio/track.mp3"), play(2, "audio/other.mp3")]);
        a.apply_script_commands(&world, &root, vec![AudioCmd::Stop { handle: 1 }]);
        assert!(!a.script_info().sounds.contains_key(&1), "a stopped sound still reads as playing");

        open.send(()).unwrap();
        open.send(()).unwrap();
        until_live(&mut a, &world, &root, 2);
        assert!(!a.sounds.contains_key(&1), "a sound stopped while it waited started anyway");
        assert_eq!(a.live_voices(), 1);
    }

    /// A long track can be bigger than the whole idle budget on its own. It is
    /// idle for the moment between arriving and its voice starting, and must
    /// not be dropped in that moment.
    #[test]
    fn a_clip_bigger_than_the_whole_idle_budget_still_plays() {
        let root = project("huge", &["long.ogg"]);
        let world = World::default();
        let mut a = system(fixed(100_000));
        a.clips = ClipCache::with_decoder(fixed(100_000), 1_000);
        a.apply_script_commands(&world, &root, vec![play(1, "audio/long.ogg")]);
        until_live(&mut a, &world, &root, 1);
    }

    /// A dedicated server runs the game's `audio.play` calls and has no device.
    /// It used to decode every clip it was asked for, music included, and
    /// then throw the result away.
    #[test]
    fn a_host_with_no_output_decodes_nothing() {
        let root = project("silent", &["track.mp3"]);
        let world = World::default();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let inner = fixed(16);
        let decode: crate::audio_clips::Decoder = std::sync::Arc::new(move |p: &Path| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            inner(p)
        });
        let mut a = AudioSystem {
            engine_failed: true,
            clips: ClipCache::with_decoder(decode, IDLE_BUDGET_BYTES),
            ..Default::default()
        };
        a.apply_script_commands(&world, &root, vec![play(1, "audio/track.mp3")]);
        a.preload(&root, "audio/track.mp3");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "decoded with nothing to play it on");
        assert_eq!(a.clip_status("audio/track.mp3"), Some(true), "a preload must not wait forever on a server");
    }
}
