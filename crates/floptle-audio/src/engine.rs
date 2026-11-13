//! The device backend: a cpal output stream owning an [`AudioCore`] on the
//! audio thread, and [`AudioEngine`] — the control-side handle the editor and
//! Lua talk to. Commands flow in through a channel; lightweight status
//! (playing voices, finished ids, track meters) flows back through shared
//! state the callback refreshes with `try_lock` so it never blocks the mix.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

#[cfg(not(target_arch = "wasm32"))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use glam::DVec3;

use crate::clip::ClipRef;
use crate::mixer::MixerDesc;
use crate::source::PlayParams;
use crate::spatial::Listener;
use crate::voice::{AudioCore, VoiceId, VoiceStatus, MAX_GROUPS};

/// Internal mixing block size (frames). Small enough for snappy control
/// response, big enough to amortize per-block spatialization.
const BLOCK: usize = 128;

enum Cmd {
    Play { id: VoiceId, clip: ClipRef, emitter: Option<DVec3>, params: Box<PlayParams> },
    PlayStream {
        id: VoiceId,
        ring: crate::stream::StreamRef,
        emitter: Option<DVec3>,
        params: Box<PlayParams>,
    },
    Stop(VoiceId),
    StopAll,
    Pause(VoiceId, bool),
    Move(VoiceId, DVec3),
    Seek(VoiceId, f32),
    Params(VoiceId, Box<PlayParams>),
    Listener(Listener),
    Mixer(Box<MixerDesc>),
    Reset,
    /// A voice group's name, sent once before anything uses it.
    Group(String),
    /// A group's cap, by its place in the list (see [`AudioEngine::group`]).
    GroupLimit(u16, Option<usize>),
}

struct VoiceEntry {
    st: VoiceStatus,
    /// The audio thread has seen this voice at least once. Until then the
    /// entry is the control side's optimistic insert and must not be reaped
    /// (the Play command may still be sitting in the queue).
    seen: bool,
}

#[derive(Default)]
struct Status {
    voices: Mutex<HashMap<VoiceId, VoiceEntry>>,
    finished: Mutex<Vec<VoiceId>>,
    meters: Mutex<Vec<(String, f32)>>,
    active: AtomicUsize,
    /// Voices playing per voice group, in the order the groups were named.
    /// The names stay on the control side (`AudioEngine::group_names`), so
    /// publishing this never copies a string on the audio thread.
    groups: Mutex<Vec<u32>>,
}

impl Status {
    fn new() -> Self {
        Self { groups: Mutex::new(Vec::with_capacity(MAX_GROUPS)), ..Default::default() }
    }
}

/// The thing that owns the output. On a desktop that is a cpal stream on its
/// own audio thread; in a browser it is our own Web Audio scheduler, for the
/// reasons `web_out` gives.
#[cfg(not(target_arch = "wasm32"))]
type OutStream = cpal::Stream;
#[cfg(target_arch = "wasm32")]
type OutStream = crate::web_out::WebStream;

/// Control-side handle to the running audio stack. Owns the output stream —
/// drop it and the sound stops.
pub struct AudioEngine {
    /// `None` for a [`silent`](Self::silent) engine.
    _stream: Option<OutStream>,
    tx: Sender<Cmd>,
    status: Arc<Status>,
    next_id: VoiceId,
    /// Every voice group named this play session, in the order the audio
    /// thread was told them: a group's index here is its index there.
    group_names: Mutex<Vec<String>>,
    /// Device output rate.
    pub sample_rate: u32,
}

/// The mix itself: interleave `channels` of [`AudioCore`] into whatever
/// buffer the output hands over. Shared by both backends so the desktop and
/// the browser can never drift into rendering differently.
fn renderer(
    rx: Receiver<Cmd>,
    status_cb: Arc<Status>,
    sample_rate: u32,
    channels: usize,
) -> impl FnMut(&mut [f32]) {
    let mut core = AudioCore::new(sample_rate as f32, BLOCK);
    let mut plan_l = vec![0.0f32; BLOCK];
    let mut plan_r = vec![0.0f32; BLOCK];
    let mut finished_scratch: Vec<VoiceId> = Vec::new();
    let mut meters_scratch: Vec<(String, f32)> = Vec::new();
    move |data: &mut [f32]| {
        drain_commands(&rx, &mut core);
        // Render planar in block chunks, interleave into `data`.
        let frames = data.len() / channels.max(1);
        let mut at = 0;
        while at < frames {
            let n = (frames - at).min(BLOCK);
            plan_l[..n].fill(0.0);
            plan_r[..n].fill(0.0);
            core.render(&mut plan_l[..n], &mut plan_r[..n]);
            for i in 0..n {
                let frame = &mut data[(at + i) * channels..(at + i + 1) * channels];
                match channels {
                    1 => frame[0] = (plan_l[i] + plan_r[i]) * 0.5,
                    _ => {
                        frame[0] = plan_l[i];
                        frame[1] = plan_r[i];
                        for s in frame.iter_mut().skip(2) {
                            *s = 0.0;
                        }
                    }
                }
            }
            at += n;
        }
        publish_status(&status_cb, &mut core, &mut finished_scratch, &mut meters_scratch);
    }
}

impl AudioEngine {
    /// Open the default output device. Fails cleanly on headless machines —
    /// callers treat audio as optional.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device =
            host.default_output_device().ok_or_else(|| "no audio output device".to_string())?;
        let config = device
            .default_output_config()
            .map_err(|e| format!("no default output config: {e}"))?;
        if config.sample_format() != cpal::SampleFormat::F32 {
            // Every mainstream backend (PipeWire/ALSA, WASAPI, CoreAudio)
            // offers f32; refusing the exotic case keeps the callback simple.
            return Err(format!("output device is not f32 ({})", config.sample_format()));
        }
        let config: cpal::StreamConfig = config.into();
        let sample_rate = config.sample_rate;
        let channels = config.channels as usize;

        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let status = Arc::new(Status::new());
        let status_cb = Arc::clone(&status);
        let mut render = renderer(rx, status_cb, sample_rate, channels);

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| render(data),
                |e| log::warn!("audio stream error: {e}"),
                None,
            )
            .map_err(|e| format!("failed to open audio stream: {e}"))?;
        stream.play().map_err(|e| format!("failed to start audio stream: {e}"))?;

        log::info!("audio: {sample_rate} Hz, {channels} ch, block {BLOCK}");
        Ok(Self { _stream: Some(stream), tx, status, next_id: 1, group_names: Mutex::default(), sample_rate })
    }

    /// Open the page's audio output. Stereo, at whatever rate the browser's
    /// own context runs at — see [`crate::web_out`] for why the scheduling is
    /// ours rather than cpal's.
    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Result<Self, String> {
        let channels = 2usize;
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let status = Arc::new(Status::new());
        let status_cb = Arc::clone(&status);

        // The renderer is built inside, because only the context knows the
        // rate the mixer has to be built for.
        let stream = crate::web_out::start(channels, move |sample_rate| {
            renderer(rx, status_cb, sample_rate, channels)
        })?;
        let sample_rate = stream.sample_rate();

        log::info!("audio: {sample_rate} Hz, {channels} ch, block {BLOCK}");
        Ok(Self { _stream: Some(stream), tx, status, next_id: 1, group_names: Mutex::default(), sample_rate })
    }

    /// An engine with no output: every call is accepted and every voice
    /// reads as playing from the moment it starts, and nothing is ever mixed.
    /// What a test drives when the question is which voices a host started,
    /// not what they sound like.
    pub fn silent() -> Self {
        let (tx, _rx) = std::sync::mpsc::channel::<Cmd>();
        Self { _stream: None, tx, status: Arc::new(Status::new()), next_id: 1, group_names: Mutex::default(), sample_rate: 48_000 }
    }

    /// An engine with no device, mixed only when its caller asks: the
    /// returned function fills a buffer of interleaved stereo exactly as an
    /// output callback would, commands and status included. For rendering to
    /// a file, and for tests that assert on what was mixed without a sound
    /// card (or a sound) anywhere.
    pub fn offline(sample_rate: u32) -> (Self, impl FnMut(&mut [f32])) {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let status = Arc::new(Status::new());
        let render = renderer(rx, Arc::clone(&status), sample_rate, 2);
        (Self { _stream: None, tx, status, next_id: 1, group_names: Mutex::default(), sample_rate }, render)
    }

    fn send(&self, cmd: Cmd) {
        // A closed channel means the stream died; every call site treats
        // audio as best-effort, so just drop the command.
        let _ = self.tx.send(cmd);
    }

    /// Fire a sound. `emitter: None` plays it flat (UI/music); otherwise it
    /// spatializes per `params.mode`. Returns the voice handle immediately.
    pub fn play(
        &mut self,
        clip: ClipRef,
        emitter: Option<DVec3>,
        params: PlayParams,
    ) -> VoiceId {
        let id = self.next_id;
        self.next_id += 1;
        // Optimistically mark playing so `is_playing` is true before the
        // first callback runs.
        if let Ok(mut v) = self.status.voices.lock() {
            v.insert(
                id,
                VoiceEntry {
                    st: VoiceStatus { playing: true, paused: false, position_secs: 0.0 },
                    seen: false,
                },
            );
        }
        self.group(&params.group);
        self.send(Cmd::Play { id, clip, emitter, params: Box::new(params) });
        id
    }

    /// Start a voice fed by a live stream — a remote player's microphone.
    /// Identical to [`Self::play`] in every other way: it is
    /// spatialised, routed through a mixer track, and retuned with
    /// [`Self::update_params`], because it really is an ordinary voice.
    ///
    /// It never ends on its own; stop it when the speaker leaves.
    pub fn play_stream(
        &mut self,
        ring: crate::stream::StreamRef,
        emitter: Option<DVec3>,
        params: PlayParams,
    ) -> VoiceId {
        let id = self.next_id;
        self.next_id += 1;
        if let Ok(mut v) = self.status.voices.lock() {
            v.insert(
                id,
                VoiceEntry {
                    st: VoiceStatus { playing: true, paused: false, position_secs: 0.0 },
                    seen: false,
                },
            );
        }
        self.group(&params.group);
        self.send(Cmd::PlayStream { id, ring, emitter, params: Box::new(params) });
        id
    }

    pub fn stop(&self, id: VoiceId) {
        self.send(Cmd::Stop(id));
    }

    pub fn stop_all(&self) {
        self.send(Cmd::StopAll);
    }

    pub fn set_paused(&self, id: VoiceId, paused: bool) {
        self.send(Cmd::Pause(id, paused));
    }

    pub fn move_voice(&self, id: VoiceId, pos: DVec3) {
        self.send(Cmd::Move(id, pos));
    }

    pub fn seek(&self, id: VoiceId, secs: f32) {
        self.send(Cmd::Seek(id, secs));
    }

    pub fn update_params(&self, id: VoiceId, params: PlayParams) {
        self.group(&params.group);
        self.send(Cmd::Params(id, Box::new(params)));
    }

    pub fn set_listener(&self, listener: Listener) {
        self.send(Cmd::Listener(listener));
    }

    pub fn set_mixer(&self, desc: &MixerDesc) {
        self.send(Cmd::Mixer(Box::new(desc.clone())));
    }

    /// Stop everything and clear all DSP state (Play stopped).
    pub fn reset(&self) {
        if let Ok(mut names) = self.group_names.lock() {
            names.clear();
        }
        self.send(Cmd::Reset);
    }

    pub fn is_playing(&self, id: VoiceId) -> bool {
        self.status(id).map(|s| s.playing).unwrap_or(false)
    }

    /// Latest published snapshot for a voice; None once it has finished.
    pub fn status(&self, id: VoiceId) -> Option<VoiceStatus> {
        self.status.voices.lock().ok().and_then(|v| v.get(&id).map(|e| e.st))
    }

    /// Voice ids that finished since the last call.
    pub fn drain_finished(&self) -> Vec<VoiceId> {
        let ids =
            self.status.finished.lock().map(|mut f| std::mem::take(&mut *f)).unwrap_or_default();
        if !ids.is_empty() {
            // Backstop for the rare contended-callback case where a finished
            // id could otherwise leave a stale "playing" entry behind.
            if let Ok(mut v) = self.status.voices.lock() {
                for id in &ids {
                    v.remove(id);
                }
            }
        }
        ids
    }

    /// Post-fader peak per mixer track (master first) — drives the tab's
    /// meters.
    pub fn meters(&self) -> Vec<(String, f32)> {
        self.status.meters.lock().map(|m| m.clone()).unwrap_or_default()
    }

    pub fn active_voices(&self) -> usize {
        self.status.active.load(Ordering::Relaxed)
    }

    /// A voice group's place in the list, naming it to the audio thread the
    /// first time it is seen. `None` for no group, or past [`MAX_GROUPS`]
    /// (that sound plays ungrouped).
    fn group(&self, name: &str) -> Option<u16> {
        if name.is_empty() {
            return None;
        }
        let mut names = self.group_names.lock().ok()?;
        if let Some(i) = names.iter().position(|n| n == name) {
            return Some(i as u16);
        }
        if names.len() >= MAX_GROUPS {
            return None;
        }
        names.push(name.to_string());
        self.send(Cmd::Group(name.to_string()));
        Some(names.len() as u16 - 1)
    }

    /// Cap how many voices a group plays at once; `None` lifts the cap. See
    /// [`AudioCore::set_group_limit`] for which voice gives way.
    pub fn set_group_limit(&self, group: &str, limit: Option<usize>) {
        if let Some(i) = self.group(group) {
            self.send(Cmd::GroupLimit(i, limit));
        }
    }

    /// Voices playing per voice group, as of the last mixed block.
    pub fn group_counts(&self) -> Vec<(String, u32)> {
        let counts = self.status.groups.lock().map(|g| g.clone()).unwrap_or_default();
        let names = self.group_names.lock().map(|n| n.clone()).unwrap_or_default();
        names.into_iter().zip(counts).collect()
    }
}

fn drain_commands(rx: &Receiver<Cmd>, core: &mut AudioCore) {
    for cmd in rx.try_iter() {
        match cmd {
            Cmd::Play { id, clip, emitter, params } => core.play(id, clip, emitter, *params),
            Cmd::PlayStream { id, ring, emitter, params } => {
                core.play_stream(id, ring, emitter, *params)
            }
            Cmd::Stop(id) => core.stop(id),
            Cmd::StopAll => core.stop_all(),
            Cmd::Pause(id, p) => core.set_paused(id, p),
            Cmd::Move(id, pos) => core.move_voice(id, pos),
            Cmd::Seek(id, secs) => core.seek(id, secs),
            Cmd::Params(id, p) => core.update_params(id, *p),
            Cmd::Listener(l) => core.set_listener(l),
            Cmd::Mixer(desc) => core.set_mixer(&desc),
            Cmd::Reset => core.reset(),
            Cmd::Group(name) => core.add_group(name),
            Cmd::GroupLimit(i, limit) => core.set_group_slot_limit(i, limit),
        }
    }
}

fn publish_status(
    status: &Status,
    core: &mut AudioCore,
    finished: &mut Vec<VoiceId>,
    meters: &mut Vec<(String, f32)>,
) {
    core.drain_finished(finished);
    status.active.store(core.active_voices(), Ordering::Relaxed);
    if let Ok(mut v) = status.voices.try_lock() {
        for id in finished.iter() {
            v.remove(id);
        }
        v.retain(|id, e| match core.status(*id) {
            Some(now) => {
                e.st = now;
                e.seen = true;
                true
            }
            // Never seen by the core: the Play command may still be queued —
            // keep the optimistic entry alive.
            None => !e.seen,
        });
    }
    if !finished.is_empty()
        && let Ok(mut f) = status.finished.try_lock()
    {
        // If the lock was contended instead, `finished` keeps accumulating
        // and lands next callback.
        f.append(finished);
    }
    if let Ok(mut g) = status.groups.try_lock() {
        core.group_counts(&mut g);
    }
    if let Ok(mut m) = status.meters.try_lock() {
        core.mixer.meters(meters);
        std::mem::swap(&mut *m, meters);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Clip;
    use crate::source::EndBehavior;

    fn looped(group: &str, priority: i32) -> PlayParams {
        PlayParams { group: group.into(), priority, end: EndBehavior::Loop, ..Default::default() }
    }

    /// **A group is named on the control side and capped on the audio
    /// thread, and the counts come back under the names.** The engine sends
    /// each name once, ahead of the first sound or limit that uses it, so the
    /// audio thread only ever compares and counts; after a reset the next
    /// session's groups start the list over.
    #[test]
    fn groups_named_by_the_engine_are_capped_and_counted() {
        let (mut eng, mut render) = AudioEngine::offline(48_000);
        let clip: ClipRef = Arc::new(Clip { sample_rate: 48_000, channels: 1, samples: vec![0.25; 4800] });
        let mut mix = |frames: usize| render(&mut vec![0.0f32; frames * 2]);
        eng.play(clip.clone(), None, looped("steps", 0));
        eng.set_group_limit("water", Some(2));
        let low = eng.play(clip.clone(), None, looped("water", 1));
        eng.play(clip.clone(), None, looped("water", 5));
        eng.play(clip.clone(), None, looped("water", 3));
        eng.play(clip.clone(), None, PlayParams { end: EndBehavior::Loop, ..Default::default() });
        mix(2400);
        assert_eq!(eng.group_counts(), vec![("steps".to_string(), 1), ("water".to_string(), 2)]);
        mix(2400);
        assert!(eng.drain_finished().contains(&low), "the lowest-priority water voice was not replaced");
        assert!(!eng.is_playing(low));
        assert_eq!(eng.active_voices(), 4);

        eng.reset();
        mix(256);
        assert_eq!(eng.group_counts(), vec![], "a reset forgets the session's groups");
        for _ in 0..3 {
            eng.play(clip.clone(), None, looped("water", 0));
        }
        mix(256);
        assert_eq!(eng.group_counts(), vec![("water".to_string(), 3)], "the old cap of two outlived the reset");
    }
}
