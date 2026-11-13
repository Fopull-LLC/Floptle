//! Playing voices + the render core.
//!
//! [`AudioCore`] is the whole audible state of the game — voices, mixer,
//! listener — with a pure `render` function. The cpal backend owns one on the
//! audio thread; tests drive one directly. Nothing here touches a device.

use glam::DVec3;

use crate::clip::ClipRef;
use crate::mixer::{MixerDesc, MixerDsp};
use crate::source::{EndBehavior, PlayParams, SpatialMode};
use crate::spatial::{pan_gains, spatialize, Listener};
use crate::stream::{StreamRef, STREAM_RATE};

/// Handle to a playing (or finished) sound. Never reused.
pub type VoiceId = u64;

/// Hard cap on simultaneous voices; new sounds beyond it are dropped (the
/// mix is mush long before this anyway).
pub const MAX_VOICES: usize = 256;

/// How many voice groups a play session can name. The list is allocated
/// once, up front, so naming a group never grows it on the audio thread; a
/// group past this many plays ungrouped.
pub const MAX_GROUPS: usize = 64;

/// Per-voice gain smoothing (declick) — fast enough to feel instant.
const VOICE_SMOOTH_MS: f32 = 4.0;

/// Where a voice's samples come from.
///
/// A stream is not a special kind of playback bolted on beside the mixer — it
/// is a different *source* for the same voice, so a remote player's microphone
/// gets spatialisation, distance falloff, mixer routing and effects for free.
/// That is the whole reason `voice.source(peer)` can be handed to code that
/// only knows about `audio.play`: it really is the same thing.
enum Source {
    Clip(ClipRef),
    /// A live stream, plus the one-sample interpolation window the resampler
    /// needs (`prev`/`next` bracket the fractional read position).
    Stream { ring: StreamRef, prev: f32, next: f32 },
}

impl Source {
    fn clip(&self) -> Option<&ClipRef> {
        match self {
            Self::Clip(c) => Some(c),
            Self::Stream { .. } => None,
        }
    }

    /// The source's native rate, for the resampling step.
    fn sample_rate(&self) -> u32 {
        match self {
            Self::Clip(c) => c.sample_rate,
            Self::Stream { .. } => STREAM_RATE,
        }
    }
}

struct Voice {
    id: VoiceId,
    source: Source,
    /// Fractional playhead in clip frames — or, for a stream, the fraction
    /// between `prev` and `next`.
    pos: f64,
    params: PlayParams,
    track_idx: usize,
    emitter: DVec3,
    /// Emitter is meaningful (spawned with a position / attached).
    positioned: bool,
    paused: bool,
    /// Fading to silence, then done.
    stopping: bool,
    done: bool,
    cur_l: f32,
    cur_r: f32,
    /// The sends, resolved to track indices, each with the level it is
    /// gliding from; a send to a track that doesn't exist is dropped.
    sends: Vec<SendTap>,
    lowpass: LowPass,
    /// Its voice group: an index into `AudioCore::groups` plus one, so
    /// counting a group and choosing which voice gives way compares
    /// integers. 0 = in no group.
    group: u16,
}

/// A named set of voices sharing one budget (`PlayParams::group`).
struct Group {
    name: String,
    /// The most voices it plays at once; `usize::MAX` = no limit.
    limit: usize,
}

/// What reaches the listener from a sound before its track: its volume
/// times its distance gain. This is what "the quietest" means when a full
/// group gives a voice up.
fn heard_gain(params: &PlayParams, emitter: Option<DVec3>, listener: &Listener) -> f32 {
    let (mode, at) = match emitter {
        Some(at) => (params.mode, at),
        None => (SpatialMode::Flat, DVec3::ZERO),
    };
    let s = spatialize(
        mode,
        params.falloff,
        params.min_distance,
        params.max_distance,
        params.pan,
        at,
        listener,
    );
    params.volume.clamp(0.0, 4.0) * s.gain
}

impl Voice {
    fn heard(&self, listener: &Listener) -> f32 {
        heard_gain(&self.params, self.positioned.then_some(self.emitter), listener)
    }

    /// Counted against its group: playing or paused, not already fading out.
    fn holds_a_slot(&self, group: u16) -> bool {
        self.group == group && !self.stopping && !self.done
    }
}

/// One resolved send.
struct SendTap {
    track: String,
    idx: usize,
    cur: f32,
    tgt: f32,
}

/// Two one-pole low-passes in series (12 dB an octave), per ear, with a
/// coefficient that glides so a cutoff change never clicks. A coefficient of
/// 1 passes the sound through untouched.
pub(crate) struct LowPass {
    cur: f32,
    tgt: f32,
    z: [f32; 4],
}

impl LowPass {
    pub(crate) fn off() -> Self {
        Self { cur: 1.0, tgt: 1.0, z: [0.0; 4] }
    }

    /// Jump to the target cutoff instead of gliding to it.
    pub(crate) fn snap(&mut self) {
        self.cur = self.tgt;
    }

    /// The cutoff in Hz; 0 (or anything past the band) passes the sound through.
    pub(crate) fn set(&mut self, hz: f32, sample_rate: f32) {
        self.tgt = if hz > 0.0 && hz < sample_rate * 0.45 {
            1.0 - (-std::f32::consts::TAU * hz / sample_rate).exp()
        } else {
            1.0
        };
    }

    pub(crate) fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if self.cur >= 0.9999 && self.tgt >= 0.9999 {
            // Passing through: keep the state on the signal so engaging later
            // starts from where the sound is, not from silence.
            if let (Some(&a), Some(&b)) = (l.last(), r.last()) {
                self.z = [a, a, b, b];
            }
            return;
        }
        let n = l.len().max(1) as f32;
        let (start, step) = (self.cur, (self.tgt - self.cur) / n);
        let [mut l1, mut l2, mut r1, mut r2] = self.z;
        for (i, (x, y)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
            let a = start + step * i as f32;
            l1 += a * (*x - l1);
            l2 += a * (l1 - l2);
            r1 += a * (*y - r1);
            r2 += a * (r1 - r2);
            *x = l2;
            *y = r2;
        }
        self.z = [l1, l2, r1, r2];
        self.cur = self.tgt;
    }
}

/// A control-side snapshot of one voice, published after every render.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceStatus {
    pub playing: bool,
    pub paused: bool,
    /// Playhead in seconds.
    pub position_secs: f32,
}

/// Everything the audio thread mixes: voices → tracks → master.
pub struct AudioCore {
    pub sample_rate: f32,
    block: usize,
    pub mixer: MixerDsp,
    voices: Vec<Voice>,
    listener: Listener,
    smooth: f32,
    /// Voice ids that finished since the last `drain_finished`.
    finished: Vec<VoiceId>,
    /// One voice's block, before it is added to its tracks.
    tmp_l: Vec<f32>,
    tmp_r: Vec<f32>,
    /// Every voice group seen since the last reset. A voice holds its group's
    /// place in this list, so the list only grows.
    groups: Vec<Group>,
}

/// Match `params.sends` onto a voice's taps, keeping the level a surviving
/// send is at so a change glides.
fn resolve_sends(mixer: &MixerDsp, old: &[SendTap], params: &PlayParams) -> Vec<SendTap> {
    params
        .sends
        .iter()
        .filter_map(|s| {
            let idx = mixer.find_track(&s.track)?;
            let cur = old.iter().find(|o| o.track == s.track).map_or(0.0, |o| o.cur);
            Some(SendTap { track: s.track.clone(), idx, cur, tgt: s.level.clamp(0.0, 4.0) })
        })
        .collect()
}

impl AudioCore {
    pub fn new(sample_rate: f32, block: usize) -> Self {
        Self {
            sample_rate,
            block,
            mixer: MixerDsp::new(sample_rate, block),
            voices: Vec::with_capacity(MAX_VOICES),
            listener: Listener::default(),
            smooth: (-1.0 / (VOICE_SMOOTH_MS / 1000.0 * sample_rate)).exp(),
            finished: Vec::new(),
            tmp_l: vec![0.0; block],
            tmp_r: vec![0.0; block],
            groups: Vec::with_capacity(MAX_GROUPS),
        }
    }

    /// Name a voice group ahead of its first sound. The engine sends each
    /// name here once, from the control side, so the string is moved in
    /// rather than copied on the audio thread, and the list (allocated at
    /// [`MAX_GROUPS`]) never grows there either.
    pub fn add_group(&mut self, name: String) {
        if !name.is_empty()
            && self.groups.len() < MAX_GROUPS
            && !self.groups.iter().any(|g| g.name == name)
        {
            self.groups.push(Group { name, limit: usize::MAX });
        }
    }

    /// A group's place in the list plus one; 0 = no group. A name nobody
    /// added is added here, which copies it: that happens only for a core
    /// driven directly (a test, an offline render), never behind the engine,
    /// which names every group first.
    fn group_slot(&mut self, name: &str) -> u16 {
        if name.is_empty() {
            return 0;
        }
        if let Some(i) = self.groups.iter().position(|g| g.name == name) {
            return i as u16 + 1;
        }
        if self.groups.len() >= MAX_GROUPS {
            return 0;
        }
        self.groups.push(Group { name: name.to_string(), limit: usize::MAX });
        self.groups.len() as u16
    }

    /// How many voices hold a slot in `group`, and which of them gives way
    /// first: the lowest priority, then the quietest at the listener, then
    /// the oldest.
    fn weakest_in(&self, group: u16) -> (usize, Option<(usize, i32, f32)>) {
        let mut held = 0;
        let mut weakest: Option<(usize, i32, f32)> = None;
        for (i, v) in self.voices.iter().enumerate() {
            if !v.holds_a_slot(group) {
                continue;
            }
            held += 1;
            let (priority, heard) = (v.params.priority, v.heard(&self.listener));
            let weaker = match weakest {
                None => true,
                Some((_, p, h)) => priority < p || (priority == p && heard < h),
            };
            if weaker {
                weakest = Some((i, priority, heard));
            }
        }
        (held, weakest)
    }

    /// Make room in a full group for a sound of this priority and loudness.
    /// False when the newcomer is itself the one that gives way: it ranks
    /// below every voice the group is playing.
    fn make_room(&mut self, group: u16, priority: i32, heard: f32) -> bool {
        let limit = self.groups[group as usize - 1].limit;
        let (held, weakest) = self.weakest_in(group);
        if held < limit {
            return true;
        }
        let Some((i, p, h)) = weakest else { return false };
        if priority < p || (priority == p && heard < h) {
            return false;
        }
        // The same declick fade as a stop, so a stolen voice never clicks.
        self.voices[i].stopping = true;
        true
    }

    /// Cap how many voices `group` plays at once; `None` lifts the cap.
    ///
    /// At the cap, a new sound in the group takes the place of the voice
    /// ranking lowest (lowest priority, then quietest at the listener, then
    /// oldest), which fades out. A newcomer ranking below all of them is
    /// dropped instead. Lowering a cap below what is playing fades the
    /// lowest-ranking extras out at once.
    pub fn set_group_limit(&mut self, group: &str, limit: Option<usize>) {
        let slot = self.group_slot(group);
        if slot != 0 {
            self.set_group_slot_limit(slot - 1, limit);
        }
    }

    /// [`Self::set_group_limit`] by the group's place in the order groups
    /// were named (from 0), the way the engine sends it.
    pub fn set_group_slot_limit(&mut self, index: u16, limit: Option<usize>) {
        if index as usize >= self.groups.len() {
            return;
        }
        let slot = index + 1;
        let limit = limit.unwrap_or(usize::MAX);
        self.groups[slot as usize - 1].limit = limit;
        loop {
            let (held, weakest) = self.weakest_in(slot);
            match weakest {
                Some((i, _, _)) if held > limit => self.voices[i].stopping = true,
                _ => break,
            }
        }
    }

    /// Voices playing in `group` right now (paused ones included, ones
    /// fading out not).
    pub fn group_count(&self, group: &str) -> usize {
        match self.groups.iter().position(|g| g.name == group) {
            Some(i) => self.voices.iter().filter(|v| v.holds_a_slot(i as u16 + 1)).count(),
            None => 0,
        }
    }

    /// Every group's voice count, in the order the groups were named,
    /// written over `out`. Give it a buffer with room for [`MAX_GROUPS`] and
    /// it never allocates, so the audio thread publishes it every callback.
    pub fn group_counts(&self, out: &mut Vec<u32>) {
        out.clear();
        out.resize(self.groups.len(), 0);
        for v in &self.voices {
            if v.group != 0 && v.holds_a_slot(v.group) {
                out[v.group as usize - 1] += 1;
            }
        }
    }

    pub fn set_mixer(&mut self, desc: &MixerDesc) {
        self.mixer.apply(desc);
        // Re-resolve voice routing: track indices may have shifted.
        for v in &mut self.voices {
            v.track_idx = self.mixer.track_index(&v.params.track);
            v.sends = resolve_sends(&self.mixer, &v.sends, &v.params);
        }
    }

    pub fn set_listener(&mut self, listener: Listener) {
        self.listener = listener;
    }

    /// Start a voice. `emitter` = None means the sound has no world position
    /// (it plays as Flat regardless of the requested mode).
    pub fn play(&mut self, id: VoiceId, clip: ClipRef, emitter: Option<DVec3>, params: PlayParams) {
        self.start(id, Source::Clip(clip), emitter, params);
    }

    /// Start a voice fed by a live stream instead of a decoded clip — a remote
    /// player's microphone.
    ///
    /// It never finishes on its own. A clip ends when it runs out of samples;
    /// a stream running out of samples means the network is late, and a voice
    /// that ended cannot be resumed — the player would go silent mid-sentence
    /// and stay that way. It plays silence and waits, until something stops it.
    pub fn play_stream(
        &mut self,
        id: VoiceId,
        ring: StreamRef,
        emitter: Option<DVec3>,
        params: PlayParams,
    ) {
        self.start(id, Source::Stream { ring, prev: 0.0, next: 0.0 }, emitter, params);
    }

    fn start(&mut self, id: VoiceId, source: Source, emitter: Option<DVec3>, params: PlayParams) {
        if self.voices.len() >= MAX_VOICES {
            self.finished.push(id);
            return;
        }
        let group = self.group_slot(&params.group);
        if group != 0 {
            let heard = heard_gain(&params, emitter, &self.listener);
            if !self.make_room(group, params.priority, heard) {
                self.finished.push(id);
                return;
            }
        }
        let sends = resolve_sends(&self.mixer, &[], &params);
        let mut lowpass = LowPass::off();
        lowpass.set(params.lowpass, self.sample_rate);
        // A sound that starts muffled starts muffled, not gliding into it.
        lowpass.cur = lowpass.tgt;
        self.voices.push(Voice {
            id,
            source,
            pos: 0.0,
            track_idx: self.mixer.track_index(&params.track),
            sends,
            lowpass,
            group,
            params,
            emitter: emitter.unwrap_or(DVec3::ZERO),
            positioned: emitter.is_some(),
            paused: false,
            stopping: false,
            done: false,
            cur_l: 0.0,
            cur_r: 0.0,
        });
    }

    fn voice_mut(&mut self, id: VoiceId) -> Option<&mut Voice> {
        self.voices.iter_mut().find(|v| v.id == id)
    }

    /// Begin the declick fade-out; the voice reports finished when silent.
    pub fn stop(&mut self, id: VoiceId) {
        if let Some(v) = self.voice_mut(id) {
            v.stopping = true;
        }
    }

    pub fn stop_all(&mut self) {
        for v in &mut self.voices {
            v.stopping = true;
        }
    }

    pub fn set_paused(&mut self, id: VoiceId, paused: bool) {
        if let Some(v) = self.voice_mut(id) {
            v.paused = paused;
        }
    }

    pub fn move_voice(&mut self, id: VoiceId, pos: DVec3) {
        if let Some(v) = self.voice_mut(id) {
            v.emitter = pos;
            v.positioned = true;
        }
    }

    pub fn seek(&mut self, id: VoiceId, secs: f32) {
        if let Some(v) = self.voice_mut(id) {
            // A live stream has no timeline to seek within — the only audio
            // that exists is what has arrived. Ignored rather than clamped to
            // zero, which would restart the interpolation window for nothing.
            let Some(clip) = v.source.clip() else { return };
            let frames = clip.frames() as f64;
            v.pos = (secs.max(0.0) as f64 * clip.sample_rate as f64).min(frames);
        }
    }

    /// Update a voice's tunables in place (volume, pitch, pan, spatial…).
    /// The clip and id stay; routing re-resolves if the track changed.
    pub fn update_params(&mut self, id: VoiceId, params: PlayParams) {
        let idx = self.mixer.track_index(&params.track);
        let sr = self.sample_rate;
        let group = self.group_slot(&params.group);
        let Some(pos) = self.voices.iter().position(|v| v.id == id) else { return };
        let sends = resolve_sends(&self.mixer, &self.voices[pos].sends, &params);
        let v = &mut self.voices[pos];
        v.lowpass.set(params.lowpass, sr);
        v.group = group;
        v.params = params;
        v.track_idx = idx;
        v.sends = sends;
    }

    /// Snapshot a voice's state (None once fully finished and drained).
    pub fn status(&self, id: VoiceId) -> Option<VoiceStatus> {
        self.voices.iter().find(|v| v.id == id).map(|v| VoiceStatus {
            playing: !v.done && !v.paused,
            paused: v.paused,
            // A stream has no position: it is not partway through anything.
            position_secs: match v.source.clip() {
                Some(c) if c.sample_rate > 0 => (v.pos / c.sample_rate as f64) as f32,
                _ => 0.0,
            },
        })
    }

    /// Ids that finished since last call (their nodes may want to despawn).
    pub fn drain_finished(&mut self, out: &mut Vec<VoiceId>) {
        out.append(&mut self.finished);
    }

    pub fn active_voices(&self) -> usize {
        self.voices.len()
    }

    /// Stop everything immediately and clear all DSP state.
    pub fn reset(&mut self) {
        for v in self.voices.drain(..) {
            self.finished.push(v.id);
        }
        // Group limits belong to the play session that set them.
        self.groups.clear();
        self.mixer.reset();
    }

    /// Render planar stereo output. Any length — chunks internally.
    pub fn render(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
        let mut at = 0;
        let total = out_l.len().min(out_r.len());
        while at < total {
            let n = (total - at).min(self.block);
            self.render_block(n);
            let (ml, mr) = (&mut out_l[at..at + n], &mut out_r[at..at + n]);
            self.mixer.process(ml, mr);
            at += n;
        }
        // Reap finished voices after the chunk loop, not per block, to keep
        // the hot loop tight.
        let finished = &mut self.finished;
        self.voices.retain(|v| {
            if v.done {
                finished.push(v.id);
            }
            !v.done
        });
    }

    /// Mix every live voice into its track's input buffer for `n` frames,
    /// and into the tracks it sends to.
    fn render_block(&mut self, n: usize) {
        let smooth = self.smooth;
        let Self { mixer, voices, listener, tmp_l, tmp_r, sample_rate, .. } = self;
        for v in voices.iter_mut() {
            if v.done || v.paused {
                continue;
            }
            // Spatialize once per block (listener/emitters move per frame,
            // not per sample); the per-sample smoother hides the steps.
            let mode = if v.positioned { v.params.mode } else { SpatialMode::Flat };
            let s = spatialize(
                mode,
                v.params.falloff,
                v.params.min_distance,
                v.params.max_distance,
                v.params.pan,
                v.emitter,
                listener,
            );
            let vol = if v.stopping { 0.0 } else { v.params.volume.clamp(0.0, 4.0) * s.gain };
            let (pl, pr) = pan_gains(s.pan);
            let (tgt_l, tgt_r) = (vol * pl, vol * pr);

            let step = v.source.sample_rate() as f64 / *sample_rate as f64
                * v.params.pitch.clamp(0.05, 8.0) as f64;
            let looping = v.params.end == EndBehavior::Loop;
            let (buf_l, buf_r) = (&mut tmp_l[..n], &mut tmp_r[..n]);
            buf_l.fill(0.0);
            buf_r.fill(0.0);

            let mut cur_l = v.cur_l;
            let mut cur_r = v.cur_r;
            let mut pos = v.pos;
            // Two loops rather than one with a branch in it: the clip path is
            // every sound in the game and stays exactly as tight as it was.
            match &mut v.source {
                Source::Clip(clip) => {
                    let frames = clip.frames() as f64;
                    for i in 0..n {
                        if pos >= frames {
                            if looping && frames > 0.0 {
                                pos -= frames;
                            } else {
                                v.done = true;
                                break;
                            }
                        }
                        let (sl, sr) = clip.sample_at(pos);
                        cur_l = tgt_l + smooth * (cur_l - tgt_l);
                        cur_r = tgt_r + smooth * (cur_r - tgt_r);
                        buf_l[i] = sl * cur_l;
                        buf_r[i] = sr * cur_r;
                        pos += step;
                    }
                }
                // A live stream is consumed, not indexed: `pos` is the fraction
                // between the two samples bracketing the read head, and the
                // head advances by pulling from the ring. Mono in, so both ears
                // get the same sample and the panner does the placing.
                Source::Stream { ring, prev, next } => {
                    for i in 0..n {
                        while pos >= 1.0 {
                            *prev = *next;
                            // Nothing there = the network is late. Ease toward
                            // silence instead of holding the last sample, which
                            // would buzz, or jumping to zero, which would click.
                            *next = ring.pop().unwrap_or(*next * 0.5);
                            pos -= 1.0;
                        }
                        let s = *prev + (*next - *prev) * pos as f32;
                        cur_l = tgt_l + smooth * (cur_l - tgt_l);
                        cur_r = tgt_r + smooth * (cur_r - tgt_r);
                        buf_l[i] = s * cur_l;
                        buf_r[i] = s * cur_r;
                        pos += step;
                    }
                }
            }
            v.lowpass.process(buf_l, buf_r);
            // The dry path, then each wet tap. A stream's samples are pulled
            // from its ring once, above, and handed to every track here: two
            // voices on one ring would split the samples between them.
            let (dl, dr) = mixer.input(v.track_idx);
            for i in 0..n {
                dl[i] += buf_l[i];
                dr[i] += buf_r[i];
            }
            let stopping = v.stopping;
            for tap in &mut v.sends {
                let tgt = if stopping { 0.0 } else { tap.tgt };
                if tap.cur == 0.0 && tgt == 0.0 {
                    continue;
                }
                let (start, inc) = (tap.cur, (tgt - tap.cur) / n.max(1) as f32);
                let (sl, sr) = mixer.input(tap.idx);
                for i in 0..n {
                    let g = start + inc * i as f32;
                    sl[i] += buf_l[i] * g;
                    sr[i] += buf_r[i] * g;
                }
                tap.cur = tgt;
            }
            v.pos = pos;
            v.cur_l = cur_l;
            v.cur_r = cur_r;
            if v.stopping && cur_l.abs() < 1e-4 && cur_r.abs() < 1e-4 {
                v.done = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Clip;
    use std::sync::Arc;

    fn tone_clip(sr: u32, secs: f32) -> ClipRef {
        let n = (sr as f32 * secs) as usize;
        Arc::new(Clip {
            sample_rate: sr,
            channels: 1,
            samples: (0..n)
                .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin() * 0.5)
                .collect(),
        })
    }

    #[test]
    fn one_shot_plays_and_finishes() {
        let mut core = AudioCore::new(48_000.0, 128);
        core.play(1, tone_clip(48_000, 0.05), None, PlayParams::default());
        let mut l = vec![0.0f32; 4800];
        let mut r = vec![0.0f32; 4800];
        core.render(&mut l, &mut r);
        assert!(l.iter().any(|s| s.abs() > 0.01), "no audible output");
        assert_eq!(core.active_voices(), 0, "one-shot should have finished");
        let mut fin = Vec::new();
        core.drain_finished(&mut fin);
        assert_eq!(fin, vec![1]);
    }

    /// A remote player's microphone has to be an ordinary sound: spatialised,
    /// routed through a mixer track, pannable. If it needed its own playback
    /// path, none of the mixer's effects would reach it and "make the killer's
    /// voice a monster with the effects you already have" would be a rewrite.
    #[test]
    fn a_stream_plays_as_an_ordinary_voice() {
        let mut core = AudioCore::new(48_000.0, 128);
        let ring = crate::stream::StreamRing::new(8192);
        ring.push(&vec![0.5f32; 4096]);
        core.play_stream(1, Arc::clone(&ring), None, PlayParams::default());
        let (mut l, mut r) = (vec![0.0f32; 1024], vec![0.0f32; 1024]);
        core.render(&mut l, &mut r);
        assert!(l.iter().any(|s| s.abs() > 0.05), "the stream never reached the mix");
        assert_eq!(core.active_voices(), 1, "a stream does not finish on its own");
    }

    /// The failure that would be worst in the field: a late packet must not end
    /// the voice. A finished voice cannot be resumed, so the player would go
    /// silent mid-sentence and stay silent for the rest of the match.
    #[test]
    fn an_empty_stream_goes_quiet_but_never_finishes() {
        let mut core = AudioCore::new(48_000.0, 128);
        let ring = crate::stream::StreamRing::new(1024);
        core.play_stream(1, Arc::clone(&ring), None, PlayParams::default());
        let (mut l, mut r) = (vec![0.0f32; 4800], vec![0.0f32; 4800]);
        core.render(&mut l, &mut r); // 100 ms of nothing arriving
        assert_eq!(core.active_voices(), 1, "still playing, just silent");
        assert!(ring.starved() > 0, "and the starvation is on the record");

        // Audio arrives late; the same voice picks it straight back up.
        ring.push(&vec![0.6f32; 4096]);
        let (mut l2, mut r2) = (vec![0.0f32; 2048], vec![0.0f32; 2048]);
        core.render(&mut l2, &mut r2);
        assert!(l2.iter().any(|s| s.abs() > 0.05), "the voice came back");
    }

    /// Distance falloff is the whole feature for proximity voice — this is what
    /// makes hearing someone mean they are near you.
    #[test]
    fn a_positioned_stream_gets_quieter_with_distance() {
        let level = |at: f64| {
            let mut core = AudioCore::new(48_000.0, 128);
            let ring = crate::stream::StreamRing::new(16384);
            ring.push(&vec![0.5f32; 8192]);
            let mut p = PlayParams { mode: SpatialMode::Distance, ..Default::default() };
            p.min_distance = 1.0;
            p.max_distance = 40.0;
            core.play_stream(1, ring, Some(DVec3::new(at, 0.0, 0.0)), p);
            let (mut l, mut r) = (vec![0.0f32; 2048], vec![0.0f32; 2048]);
            core.render(&mut l, &mut r);
            l.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
        };
        let near = level(1.0);
        let far = level(30.0);
        assert!(near > 0.05, "a speaker at arm's length should be plainly audible: {near}");
        assert!(far < near * 0.5, "near {near} vs far {far} — distance must matter");
    }

    /// A stream is not partway through anything, and asking it to seek must not
    /// disturb it.
    #[test]
    fn a_stream_has_no_position_and_ignores_seeking() {
        let mut core = AudioCore::new(48_000.0, 128);
        let ring = crate::stream::StreamRing::new(4096);
        ring.push(&vec![0.5f32; 2048]);
        core.play_stream(7, ring, None, PlayParams::default());
        core.seek(7, 12.0);
        let st = core.status(7).expect("still playing");
        assert_eq!(st.position_secs, 0.0);
        assert!(st.playing);
    }

    fn mixer_with(tracks: &[(&str, bool)]) -> MixerDesc {
        let mut d = MixerDesc::default();
        for (name, muted) in tracks {
            let mut t = crate::mixer::TrackDesc::new(*name);
            t.muted = *muted;
            d.tracks.push(t);
        }
        d
    }

    fn peak(l: &[f32]) -> f32 {
        l.iter().map(|s| s.abs()).fold(0.0, f32::max)
    }

    /// **One stream, heard dry and in a reverb at once.** A voice sends into
    /// another track as well as its own, at the send's level, and its ring is
    /// read once: a second engine voice on the same ring would split the
    /// samples between them instead.
    #[test]
    fn a_stream_sends_to_a_second_track_and_is_read_once() {
        // Dry path muted or not, send track muted or not: each alone.
        let run = |dry_muted: bool, wet_muted: bool, send: f32| {
            let mut core = AudioCore::new(48_000.0, 128);
            core.set_mixer(&mixer_with(&[("Voice", dry_muted), ("Verb", wet_muted)]));
            let ring = crate::stream::StreamRing::new(48_000);
            ring.push(&vec![0.5f32; 24_000]);
            let mut p = PlayParams { track: "Voice".into(), ..Default::default() };
            p.set_send("Verb", send);
            core.play_stream(1, Arc::clone(&ring), None, p);
            let (mut l, mut r) = (vec![0.0f32; 4800], vec![0.0f32; 4800]);
            core.render(&mut l, &mut r);
            (peak(&l[2400..]), ring.len())
        };
        let (dry, left_dry) = run(false, true, 0.5);
        let (wet, left_wet) = run(true, false, 0.5);
        let (none, _) = run(true, false, 0.0);
        assert!(dry > 0.1, "the dry path is silent: {dry}");
        assert!((wet / dry - 0.5).abs() < 0.02, "the send is {wet} against a dry {dry}, not half");
        assert!(none < 1e-4, "no send, and the reverb track still heard it: {none}");
        assert_eq!(left_dry, left_wet, "the send read the ring a second time");
    }

    /// A send to a track that doesn't exist is dropped. Falling back to Master,
    /// as a voice's own track does, would play the sound twice as loud.
    #[test]
    fn a_send_to_a_missing_track_is_dropped_not_doubled() {
        let run = |send: Option<&str>| {
            let mut core = AudioCore::new(48_000.0, 128);
            let mut p = PlayParams::default();
            if let Some(t) = send {
                p.set_send(t, 1.0);
            }
            core.play(1, tone_clip(48_000, 0.2), None, p);
            let (mut l, mut r) = (vec![0.0f32; 4800], vec![0.0f32; 4800]);
            core.render(&mut l, &mut r);
            peak(&l[2400..])
        };
        let plain = run(None);
        let missing = run(Some("Nope"));
        assert!((missing - plain).abs() < 1e-4, "{missing} vs {plain}");
    }

    /// **The low-pass muffles.** A high tone through a 500 Hz cutoff is a
    /// fraction of itself; a low one is nearly untouched.
    #[test]
    fn a_lowpass_cuts_the_highs_and_keeps_the_lows() {
        let tone = |hz: f32| {
            let n = 48_000 / 5;
            Arc::new(Clip {
                sample_rate: 48_000,
                channels: 1,
                samples: (0..n).map(|i| (std::f32::consts::TAU * hz * i as f32 / 48_000.0).sin() * 0.5).collect(),
            })
        };
        let run = |hz: f32, lowpass: f32| {
            let mut core = AudioCore::new(48_000.0, 128);
            core.play(1, tone(hz), None, PlayParams { lowpass, ..Default::default() });
            let (mut l, mut r) = (vec![0.0f32; 4800], vec![0.0f32; 4800]);
            core.render(&mut l, &mut r);
            peak(&l[2400..])
        };
        let (high, high_cut) = (run(5000.0, 0.0), run(5000.0, 500.0));
        let (low, low_cut) = (run(100.0, 0.0), run(100.0, 500.0));
        assert!(high_cut < high * 0.05, "5 kHz through 500 Hz kept {high_cut} of {high}");
        assert!(low_cut > low * 0.9, "100 Hz through 500 Hz kept only {low_cut} of {low}");
    }

    /// **A bus low-pass muffles everything on that bus, and nothing else.**
    /// A 4 kHz tone on SFX through SFX's 500 Hz cutoff — set on a running
    /// mixer, as a script does — is a fraction of itself; the same tone on
    /// Music is untouched.
    #[test]
    fn a_bus_lowpass_cuts_a_high_tone_on_that_bus_only() {
        let tone = || {
            let n = 48_000 / 5;
            Arc::new(Clip {
                sample_rate: 48_000,
                channels: 1,
                samples: (0..n).map(|i| (std::f32::consts::TAU * 4000.0 * i as f32 / 48_000.0).sin() * 0.5).collect(),
            })
        };
        let run = |bus_lowpass: f32, track: &str| {
            let mut core = AudioCore::new(48_000.0, 128);
            let mixer = |lowpass: f32| crate::MixerDesc {
                master: crate::TrackDesc::new(crate::MASTER),
                tracks: vec![crate::TrackDesc { lowpass, ..crate::TrackDesc::new("SFX") }, crate::TrackDesc::new("Music")],
            };
            core.mixer.apply(&mixer(0.0));
            core.mixer.apply(&mixer(bus_lowpass));
            core.play(1, tone(), None, PlayParams { track: track.into(), ..Default::default() });
            let (mut l, mut r) = (vec![0.0f32; 4800], vec![0.0f32; 4800]);
            core.render(&mut l, &mut r);
            peak(&l[2400..])
        };
        let (open, cut) = (run(0.0, "SFX"), run(500.0, "SFX"));
        assert!(open > 0.1, "the fixture: the tone reaches the output ({open})");
        assert!(cut < open * 0.05, "4 kHz through SFX's 500 Hz low-pass kept {cut} of {open}");
        let other = run(500.0, "Music");
        assert!((other - open).abs() < open * 0.02, "SFX's low-pass changed Music: {other} vs {open}");
    }

    /// Twenty looping voices' worth of render, to let a stolen voice fade out
    /// and be reaped.
    fn settle(core: &mut AudioCore) {
        let (mut l, mut r) = (vec![0.0f32; 4800], vec![0.0f32; 4800]);
        core.render(&mut l, &mut r);
    }

    fn looped(group: &str, priority: i32, volume: f32) -> PlayParams {
        PlayParams {
            group: group.into(),
            priority,
            volume,
            end: EndBehavior::Loop,
            mode: SpatialMode::Flat,
            ..Default::default()
        }
    }

    fn drained(core: &mut AudioCore) -> Vec<VoiceId> {
        let mut fin = Vec::new();
        core.drain_finished(&mut fin);
        fin.sort();
        fin
    }

    /// **A full group gives up its lowest-priority voice.** Three voices in
    /// a group capped at three, a fourth arrives: the one with the lowest
    /// priority fades out and reports finished, whatever its loudness.
    #[test]
    fn a_full_group_gives_up_its_lowest_priority_voice() {
        let mut core = AudioCore::new(48_000.0, 128);
        core.set_group_limit("water", Some(3));
        core.play(1, tone_clip(48_000, 0.5), None, looped("water", 5, 0.1));
        core.play(2, tone_clip(48_000, 0.5), None, looped("water", 1, 1.0));
        core.play(3, tone_clip(48_000, 0.5), None, looped("water", 5, 0.2));
        assert_eq!(core.group_count("water"), 3);
        core.play(4, tone_clip(48_000, 0.5), None, looped("water", 2, 0.5));
        assert_eq!(core.group_count("water"), 3, "the group went over its limit");
        settle(&mut core);
        assert_eq!(drained(&mut core), vec![2], "the loud priority-1 voice is the one that goes");
        assert_eq!(core.active_voices(), 3);
    }

    /// **Among equals, the quietest at the listener goes** — by what the
    /// listener gets, so a loud sound far away loses to a soft one close by.
    #[test]
    fn among_equal_priorities_the_quietest_at_the_listener_goes() {
        let mut core = AudioCore::new(48_000.0, 128);
        core.set_group_limit("hits", Some(2));
        let spatial = |volume: f32| PlayParams {
            group: "hits".into(),
            volume,
            end: EndBehavior::Loop,
            min_distance: 1.0,
            max_distance: 100.0,
            falloff: crate::Falloff::Linear,
            ..Default::default()
        };
        // Voice 1: half volume, at the listener (heard at 0.5). It is the
        // oldest, so an order that ignored loudness would pick it.
        core.play(1, tone_clip(48_000, 0.5), Some(DVec3::new(0.5, 0.0, 0.0)), spatial(0.5));
        // Voice 2: full volume, 80 units away (heard at about 0.2).
        core.play(2, tone_clip(48_000, 0.5), Some(DVec3::new(80.0, 0.0, 0.0)), spatial(1.0));
        core.play(3, tone_clip(48_000, 0.5), Some(DVec3::new(0.5, 0.0, 0.0)), spatial(0.4));
        settle(&mut core);
        assert_eq!(drained(&mut core), vec![2], "the far voice is the quietest heard, though the loudest played");
    }

    /// **A newcomer ranking below the whole group is dropped**, and nothing
    /// playing is touched: by priority, and among equals by loudness. An
    /// equal newcomer wins, taking the oldest equal's place.
    #[test]
    fn a_newcomer_below_the_whole_group_is_dropped() {
        let mut core = AudioCore::new(48_000.0, 128);
        core.set_group_limit("water", Some(2));
        core.play(1, tone_clip(48_000, 0.5), None, looped("water", 3, 0.5));
        core.play(2, tone_clip(48_000, 0.5), None, looped("water", 3, 0.5));
        core.play(3, tone_clip(48_000, 0.5), None, looped("water", 2, 1.0));
        assert_eq!(drained(&mut core), vec![3], "a lower-priority newcomer is the one dropped");
        core.play(4, tone_clip(48_000, 0.5), None, looped("water", 3, 0.25));
        assert_eq!(drained(&mut core), vec![4], "an equal-priority, quieter newcomer is dropped");
        settle(&mut core);
        assert_eq!(drained(&mut core), Vec::<VoiceId>::new(), "a dropped newcomer cost a playing voice");
        assert_eq!(core.group_count("water"), 2);
        core.play(5, tone_clip(48_000, 0.5), None, looped("water", 3, 0.5));
        settle(&mut core);
        assert_eq!(drained(&mut core), vec![1], "an equal newcomer takes the oldest equal's place");
    }

    /// **A limit is its group's alone, and no limit is the old behaviour.**
    /// A capped group does not count or steal another group's voices or
    /// ungrouped ones; a group with no cap, or a cap lifted, plays everything.
    #[test]
    fn a_limit_touches_only_its_own_group() {
        let mut core = AudioCore::new(48_000.0, 128);
        core.set_group_limit("water", Some(1));
        core.play(1, tone_clip(48_000, 0.5), None, looped("", 0, 1.0));
        core.play(2, tone_clip(48_000, 0.5), None, looped("steps", -5, 0.1));
        core.play(3, tone_clip(48_000, 0.5), None, looped("steps", -5, 0.1));
        core.play(4, tone_clip(48_000, 0.5), None, looped("water", 0, 1.0));
        settle(&mut core);
        assert_eq!(drained(&mut core), Vec::<VoiceId>::new(), "a voice outside the capped group was stolen");
        assert_eq!((core.group_count("water"), core.group_count("steps"), core.group_count("nope")), (1, 2, 0));
        let mut counts = Vec::new();
        core.group_counts(&mut counts);
        assert_eq!(counts, vec![1, 2], "water, then steps: the order they were named");

        core.set_group_limit("water", None);
        core.play(5, tone_clip(48_000, 0.5), None, looped("water", 0, 1.0));
        settle(&mut core);
        assert_eq!(core.group_count("water"), 2, "a lifted cap still held");
        assert_eq!(drained(&mut core), Vec::<VoiceId>::new());
    }

    /// **Lowering a cap below what is playing fades the extras out**, lowest
    /// rank first; and a cap of zero plays nothing.
    #[test]
    fn lowering_a_limit_fades_out_the_lowest_ranked_extras() {
        let mut core = AudioCore::new(48_000.0, 128);
        for (id, priority) in [(1, 4), (2, 1), (3, 3), (4, 2)] {
            core.play(id, tone_clip(48_000, 0.5), None, looped("crowd", priority, 1.0));
        }
        core.set_group_limit("crowd", Some(2));
        assert_eq!(core.group_count("crowd"), 2);
        settle(&mut core);
        assert_eq!(drained(&mut core), vec![2, 4]);
        core.set_group_limit("crowd", Some(0));
        core.play(9, tone_clip(48_000, 0.5), None, looped("crowd", 99, 1.0));
        settle(&mut core);
        assert_eq!(drained(&mut core), vec![1, 3, 9]);
        assert_eq!(core.active_voices(), 0);
    }

    /// **A stolen voice fades; it is not cut.** The sample-to-sample step of
    /// the mix across the steal stays far below the tone's own amplitude.
    #[test]
    fn a_stolen_voice_fades_out_without_a_click() {
        let mut core = AudioCore::new(48_000.0, 128);
        core.set_group_limit("one", Some(1));
        // A constant signal, so any step in the output is the gain moving.
        let dc = || Arc::new(Clip { sample_rate: 48_000, channels: 1, samples: vec![0.5; 48_000] });
        core.play(1, dc(), None, looped("one", 0, 1.0));
        let (mut l, mut r) = (vec![0.0f32; 2400], vec![0.0f32; 2400]);
        core.render(&mut l, &mut r);
        let before = *l.last().unwrap();
        assert!(before > 0.4, "the fixture: voice 1 is at full level ({before})");
        // The newcomer outranks it and is silent, so what is heard next is
        // voice 1 leaving.
        core.play(2, dc(), None, looped("one", 1, 0.0));
        let (mut l2, mut r2) = (vec![0.0f32; 4800], vec![0.0f32; 4800]);
        core.render(&mut l2, &mut r2);
        let mut prev = before;
        let mut worst = 0.0f32;
        for s in &l2 {
            worst = worst.max((s - prev).abs());
            prev = *s;
        }
        assert!(worst < 0.02, "the steal stepped the output by {worst}");
        assert!(l2[4799].abs() < 1e-3, "the stolen voice never went quiet");
        assert_eq!(drained(&mut core), vec![1]);
    }

    #[test]
    fn looping_voice_keeps_playing() {
        let mut core = AudioCore::new(48_000.0, 128);
        let params = PlayParams { end: EndBehavior::Loop, ..Default::default() };
        core.play(7, tone_clip(48_000, 0.01), None, params);
        let mut l = vec![0.0f32; 9600];
        let mut r = vec![0.0f32; 9600];
        core.render(&mut l, &mut r);
        assert_eq!(core.active_voices(), 1, "looping voice ended");
        assert!(l[9000..].iter().any(|s| s.abs() > 0.01), "loop went silent");
        core.stop(7);
        core.render(&mut l, &mut r);
        assert_eq!(core.active_voices(), 0, "stop did not end the loop");
    }

    #[test]
    fn distance_quiets_spatial_voice() {
        let render_at = |d: f64| {
            let mut core = AudioCore::new(48_000.0, 128);
            core.play(1, tone_clip(48_000, 0.5), Some(DVec3::new(d, 0.0, 0.0)), PlayParams::default());
            let mut l = vec![0.0f32; 9600];
            let mut r = vec![0.0f32; 9600];
            core.render(&mut l, &mut r);
            l.iter().zip(r.iter()).map(|(a, b)| a.abs().max(b.abs())).fold(0.0f32, f32::max)
        };
        let near = render_at(1.0);
        let mid = render_at(25.0);
        let far = render_at(100.0); // past default max_distance (50)
        assert!(near > mid && mid > far, "attenuation not monotonic: {near} {mid} {far}");
        assert!(far < 1e-3, "outside max_distance should be silent, got {far}");
    }

    #[test]
    fn different_sample_rate_clip_keeps_duration() {
        // A 22.05 kHz clip on a 48 kHz engine must still last its real time.
        let mut core = AudioCore::new(48_000.0, 128);
        core.play(1, tone_clip(22_050, 0.1), None, PlayParams::default());
        let mut l = vec![0.0f32; 3600]; // 75 ms
        let mut r = vec![0.0f32; 3600];
        core.render(&mut l, &mut r);
        assert_eq!(core.active_voices(), 1, "clip ended early — resample step wrong");
        let mut l2 = vec![0.0f32; 2400]; // through 125 ms total
        let mut r2 = vec![0.0f32; 2400];
        core.render(&mut l2, &mut r2);
        assert_eq!(core.active_voices(), 0, "clip overran its duration");
    }
}
