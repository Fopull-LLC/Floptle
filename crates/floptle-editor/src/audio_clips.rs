//! Decoded clips, kept off the frame.
//!
//! A clip is decoded on a worker the first time anything asks for it, and a
//! sound that wanted it waits (reporting that it is playing) until it is in.
//! Decoding on the frame froze a game for the whole file: about a millisecond
//! for a gunshot, and 111–186 ms for a two-to-three-minute music track, once
//! per track.
//!
//! Decoded audio is f32 PCM, about 50 MB for a two-and-a-half-minute stereo
//! track, so it is not kept for the whole session either. A clip no voice is
//! playing is idle; idle clips are kept up to [`IDLE_BUDGET_BYTES`] and the
//! least recently played goes first. A clip a script preloaded stays until its
//! first play, whatever the budget, because dropping it early would undo the
//! one thing the preload was for.
//!
//! The cache only drops a clip it holds the last reference to, so the free
//! happens here, on the main thread, and never on the audio thread in the
//! middle of a mix.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;

use floptle_audio::{Clip, ClipRef};

/// How much idle decoded audio is kept for the next play: room for one idle
/// music track beside every sound effect a game is likely to have.
pub(crate) const IDLE_BUDGET_BYTES: usize = 64 << 20;

/// Turns a file into PCM. The real one is [`floptle_audio::load_clip`]; a
/// test swaps in one it can hold back.
pub(crate) type Decoder = Arc<dyn Fn(&Path) -> Result<Clip, String> + Send + Sync>;

/// What a caller can do with a clip right now.
pub(crate) enum ClipState {
    Ready(ClipRef),
    /// On a worker; ask again next frame.
    Loading,
    /// Missing or undecodable. Said once, when it was found out.
    Failed,
}

enum Slot {
    Loading { rx: Receiver<Result<Clip, String>>, pinned: bool },
    Ready { clip: ClipRef, used: u64, pinned: bool },
    Failed,
}

pub(crate) struct ClipCache {
    slots: HashMap<String, Slot>,
    decode: Decoder,
    /// Bumped on every play, so "least recently played" is an order.
    clock: u64,
    budget: usize,
}

impl Default for ClipCache {
    fn default() -> Self {
        Self::with_decoder(Arc::new(|p: &Path| floptle_audio::load_clip(p)), IDLE_BUDGET_BYTES)
    }
}

fn bytes(c: &Clip) -> usize {
    c.samples.len() * std::mem::size_of::<f32>()
}

impl ClipCache {
    pub(crate) fn with_decoder(decode: Decoder, budget: usize) -> Self {
        Self { slots: HashMap::new(), decode, clock: 0, budget }
    }

    /// The clip for a play: ready, or started on a worker if nothing has
    /// asked for it yet. Counts as a use, which takes a preload's pin off.
    pub(crate) fn get(&mut self, root: &Path, key: &str) -> ClipState {
        self.request(root, key, false);
        self.clock += 1;
        match self.slots.get_mut(key) {
            Some(Slot::Ready { clip, used, pinned }) => {
                *used = self.clock;
                *pinned = false;
                ClipState::Ready(clip.clone())
            }
            Some(Slot::Loading { .. }) => ClipState::Loading,
            Some(Slot::Failed) | None => ClipState::Failed,
        }
    }

    /// What [`get`](Self::get) would answer, without asking for anything or
    /// counting a use.
    pub(crate) fn peek(&self, key: &str) -> ClipState {
        match self.slots.get(key) {
            Some(Slot::Ready { clip, .. }) => ClipState::Ready(clip.clone()),
            Some(Slot::Loading { .. }) => ClipState::Loading,
            Some(Slot::Failed) | None => ClipState::Failed,
        }
    }

    /// Start decoding ahead of the first play, and keep the result until then.
    pub(crate) fn preload(&mut self, root: &Path, key: &str) {
        self.request(root, key, true);
        match self.slots.get_mut(key) {
            Some(Slot::Ready { pinned, .. } | Slot::Loading { pinned, .. }) => *pinned = true,
            Some(Slot::Failed) | None => {}
        }
    }

    /// `Some(true)` in, `Some(false)` failed, `None` still decoding: what a
    /// preload waits on.
    pub(crate) fn status(&self, key: &str) -> Option<bool> {
        match self.slots.get(key) {
            Some(Slot::Ready { .. }) => Some(true),
            Some(Slot::Loading { .. }) => None,
            Some(Slot::Failed) | None => Some(false),
        }
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.slots.values().any(|s| matches!(s, Slot::Loading { .. }))
    }

    fn request(&mut self, root: &Path, key: &str, pinned: bool) {
        if key.is_empty() || self.slots.contains_key(key) {
            return;
        }
        let Some(path) = resolve_clip_path(root, key) else {
            log::warn!("audio: clip not found: {key}");
            self.slots.insert(key.to_string(), Slot::Failed);
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let decode = self.decode.clone();
        crate::worker::spawn("floptle-audio-decode", move || {
            let _ = tx.send(decode(&path));
        });
        self.slots.insert(key.to_string(), Slot::Loading { rx, pinned });
    }

    /// Take in every clip a worker has finished. Nothing is dropped here: a
    /// clip that just came in is idle until the sound waiting for it starts,
    /// so [`evict`](Self::evict) runs after the waiters have been started.
    pub(crate) fn receive(&mut self) {
        let clock = self.clock;
        for (key, slot) in self.slots.iter_mut() {
            let Slot::Loading { rx, pinned } = slot else { continue };
            let pinned = *pinned;
            *slot = match rx.try_recv() {
                Ok(Ok(c)) => Slot::Ready { clip: Arc::new(c), used: clock, pinned },
                Ok(Err(e)) => {
                    log::warn!("audio: {e}");
                    Slot::Failed
                }
                Err(TryRecvError::Empty) => continue,
                Err(TryRecvError::Disconnected) => {
                    log::warn!("audio: {key}: the decode worker stopped");
                    Slot::Failed
                }
            };
        }
    }

    /// Drop the least recently played idle clips until the idle ones fit.
    /// Idle means nothing but this cache holds it; a pinned clip is not
    /// idle. Once a frame; it allocates only when there is something to drop.
    pub(crate) fn evict(&mut self) {
        let is_idle = |s: &Slot| match s {
            Slot::Ready { clip, pinned: false, .. } if Arc::strong_count(clip) == 1 => Some(bytes(clip)),
            _ => None,
        };
        let mut held: usize = self.slots.values().filter_map(is_idle).sum();
        if held <= self.budget {
            return;
        }
        let mut idle: Vec<(u64, String, usize)> = self
            .slots
            .iter()
            .filter_map(|(k, s)| match s {
                Slot::Ready { clip, used, pinned: false } if Arc::strong_count(clip) == 1 => {
                    Some((*used, k.clone(), bytes(clip)))
                }
                _ => None,
            })
            .collect();
        idle.sort();
        for (_, key, b) in idle {
            if held <= self.budget {
                break;
            }
            self.slots.remove(&key);
            held -= b;
        }
    }

    /// Decoded bytes this cache holds, playing or not.
    #[cfg(test)]
    pub(crate) fn resident_bytes(&self) -> usize {
        self.slots.values().map(|s| if let Slot::Ready { clip, .. } = s { bytes(clip) } else { 0 }).sum()
    }
}

/// A clip reference to a file: the project-relative path as written, else
/// with each audio extension appended (so Lua can say `"audio/hit"`).
fn resolve_clip_path(root: &Path, key: &str) -> Option<PathBuf> {
    // resolve_asset_path handles every ref spelling (project-relative,
    // legacy `assets/…`-prefixed, CWD-relative, absolute) — probe the key
    // as written, then with each audio extension appended.
    let direct = crate::project::resolve_asset_path(root, key);
    if floptle_vfs::is_file(&direct) {
        return Some(direct);
    }
    for ext in floptle_audio::AUDIO_EXTENSIONS {
        let p = crate::project::resolve_asset_path(root, &format!("{key}.{ext}"));
        if floptle_vfs::is_file(&p) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender};
    use std::time::{Duration, Instant};

    /// A project with an (empty) file per name — the decoders here never read
    /// it, but the cache only asks a worker for a file that exists.
    pub(crate) fn project(tag: &str, names: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("floptle-audio-clips-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("audio")).unwrap();
        for n in names {
            std::fs::write(root.join("audio").join(n), b"").unwrap();
        }
        root
    }

    /// A clip of `n` samples, whatever the file says.
    pub(crate) fn fixed(n: usize) -> Decoder {
        Arc::new(move |_: &Path| Ok(Clip { sample_rate: 48_000, channels: 1, samples: vec![0.0; n] }))
    }

    /// A decoder that does not finish until the test says so, and says which
    /// thread it ran on.
    pub(crate) fn gated(n: usize) -> (Decoder, Sender<()>, Receiver<std::thread::ThreadId>) {
        let (open, gate) = std::sync::mpsc::channel::<()>();
        let (said, ran_on) = std::sync::mpsc::channel();
        let gate = Mutex::new(gate);
        let said = Mutex::new(said);
        let d: Decoder = Arc::new(move |_: &Path| {
            let _ = said.lock().unwrap().send(std::thread::current().id());
            let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
            Ok(Clip { sample_rate: 48_000, channels: 1, samples: vec![0.0; n] })
        });
        (d, open, ran_on)
    }

    fn settle(c: &mut ClipCache) {
        let t = Instant::now();
        while c.is_loading() && t.elapsed() < Duration::from_secs(5) {
            c.receive();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!c.is_loading(), "a decode never came back");
    }

    #[test]
    fn a_first_play_returns_before_the_clip_is_decoded_and_decodes_elsewhere() {
        let root = project("first", &["music.ogg"]);
        let (decode, open, ran_on) = gated(8);
        let mut c = ClipCache::with_decoder(decode, IDLE_BUDGET_BYTES);

        let t = Instant::now();
        let first = c.get(&root, "audio/music.ogg");
        assert!(
            matches!(first, ClipState::Loading),
            "the first play waited for the decode (took {:?})",
            t.elapsed()
        );
        let thread = ran_on.recv_timeout(Duration::from_secs(5)).expect("the decode never started");
        assert_ne!(thread, std::thread::current().id(), "decoded on the thread that asked");

        open.send(()).unwrap();
        settle(&mut c);
        assert!(matches!(c.get(&root, "audio/music.ogg"), ClipState::Ready(_)));
        // An extensionless name resolves the way `audio.play` spells it.
        assert!(matches!(c.get(&root, "audio/music"), ClipState::Loading | ClipState::Ready(_)));
        assert!(matches!(c.get(&root, "audio/nothing"), ClipState::Failed));        let _ = open.send(());
    }

    #[test]
    fn idle_clips_past_the_budget_go_least_recently_played_first() {
        let root = project("lru", &["a.wav", "b.wav", "c.wav", "held.wav"]);
        // 1000 samples = 4000 bytes each; room for two idle clips.
        let mut c = ClipCache::with_decoder(fixed(1000), 8_000);
        for k in ["audio/a.wav", "audio/b.wav", "audio/c.wav", "audio/held.wav"] {
            c.get(&root, k);
        }
        settle(&mut c);
        // Played in the order held, b, a, c: `b` is now the least recent idle one.
        let ClipState::Ready(held) = c.get(&root, "audio/held.wav") else { panic!("held not in") };
        for k in ["audio/b.wav", "audio/a.wav", "audio/c.wav"] {
            c.get(&root, k);
        }
        c.evict();
        assert_eq!(c.status("audio/b.wav"), Some(false), "the least recently played idle clip stayed");
        assert_eq!(c.status("audio/a.wav"), Some(true));
        assert_eq!(c.status("audio/c.wav"), Some(true));
        assert_eq!(c.status("audio/held.wav"), Some(true), "a clip a voice is playing was dropped");
        assert_eq!(c.resident_bytes(), 12_000);

        // Its voice ends: now it is idle, and the oldest.
        drop(held);
        c.evict();
        assert_eq!(c.status("audio/held.wav"), Some(false));
        assert_eq!(c.resident_bytes(), 8_000);
    }

    #[test]
    fn a_preloaded_clip_stays_until_its_first_play_whatever_the_budget() {
        let root = project("pin", &["big.ogg"]);
        let mut c = ClipCache::with_decoder(fixed(10_000), 1_000);
        c.preload(&root, "audio/big.ogg");
        settle(&mut c);
        c.evict();
        assert_eq!(c.status("audio/big.ogg"), Some(true), "a preload was dropped before it was ever played");

        let ClipState::Ready(voice) = c.get(&root, "audio/big.ogg") else { panic!("not in") };
        c.evict();
        assert_eq!(c.status("audio/big.ogg"), Some(true), "dropped while playing");
        drop(voice);
        c.evict();
        assert_eq!(c.status("audio/big.ogg"), Some(false), "a played, idle clip over the budget stayed");
    }
}
