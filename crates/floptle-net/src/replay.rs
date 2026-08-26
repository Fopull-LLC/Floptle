//! The session input log (`docs/multiplayer.md` §5) — what makes
//! match replays and the referee the same feature wearing different hats.
//!
//! A rollback match is a pure function of (scene, match seed, input delay,
//! roster, every peer's input per applied tick). Nothing else gets in: no wall
//! clock, no unseeded RNG, no frame-rate dependence — that is the determinism
//! contract the whole design rests on. So the inputs **are** the replay file,
//! and playback is not playback at all, it is running the match again.
//!
//! That single fact buys three things off one log:
//!
//! - **Match replays.** Kilobytes for a full match, and the replay is the match
//!   rather than a recording of it — you can step it, watch it from another
//!   camera, or diff two runs of it.
//! - **The referee.** The host runs the same simulation at the confirmed
//!   frontier only, never guessing and never rolling back, and holds the
//!   authoritative result. A client reporting a different checksum is either
//!   desynced or lying, and from the referee's side those look the same, which
//!   is exactly what you want from anti-cheat.
//! - **Spectators and late joiners** (future): the log plus a keyframe.
//!
//! The log is only meaningful to a build that agrees about all of it, so it
//! records `proto` and the input-map hash and refuses to load into anything
//! else. Actions are indexed positionally on the wire; a log replayed against a
//! differently-ordered `input.ron` would not fail, it would silently play a
//! different match.

use serde::{Deserialize, Serialize};

use crate::transport::PeerId;
use crate::wire::{NetInput, PROTO_VERSION};

/// One peer's input for one applied tick.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LogEntry {
    pub tick: u64,
    pub peer: PeerId,
    pub input: NetInput,
}

/// Everything needed to run a match again.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct InputLog {
    /// The build that recorded it. A log from another protocol version cannot
    /// be trusted to mean the same thing.
    pub proto: u16,
    /// Project-relative scene path — the world the match was played in.
    pub scene: String,
    /// The host's match seed: every `net.random()` draw comes from it.
    pub seed: u64,
    pub input_delay: u8,
    /// Slot order. Slot *n* is `peers[n]`, and slot order is what the scene's
    /// `Rollback` nodes are matched against — so it has to be recorded, not
    /// re-derived.
    pub peers: Vec<PeerId>,
    /// The recording build's `input.ron` hash. Actions ride the wire by
    /// POSITION, so a log played against a differently-ordered map plays a
    /// different match without erroring anywhere.
    pub input_map_hash: u64,
    /// Sorted by `(tick, peer)` — arrival order is a property of the network,
    /// not of the match, and two recordings of one match must be identical
    /// files.
    pub entries: Vec<LogEntry>,
}

/// Why a log can't be played by this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogError {
    Proto { theirs: u16, ours: u16 },
    InputMap,
    Parse(String),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proto { theirs, ours } => write!(
                f,
                "replay was recorded by protocol {theirs}, this build speaks {ours} — the \
                 simulation has changed underneath it and it would play a different match"
            ),
            Self::InputMap => write!(
                f,
                "replay's input.ron differs from this project's — actions ride the log by \
                 POSITION, so this would play silently wrong rather than fail"
            ),
            Self::Parse(e) => write!(f, "replay file is not readable: {e}"),
        }
    }
}

impl InputLog {
    pub fn new(scene: &str, seed: u64, input_delay: u8, peers: Vec<PeerId>, map: u64) -> Self {
        Self {
            proto: PROTO_VERSION,
            scene: scene.to_string(),
            seed,
            input_delay,
            peers,
            input_map_hash: map,
            entries: Vec::new(),
        }
    }

    /// Record one input, ignoring duplicates.
    ///
    /// The redundant input window re-carries recent ticks in every packet, so
    /// the recorder sees the same `(peer, tick)` many times. Keeping them all
    /// would not change playback — the driver ignores a repeat — but it would
    /// mean two recordings of one match were different files, and a replay
    /// format you cannot compare is one you cannot debug with.
    pub fn record(&mut self, peer: PeerId, tick: u64, input: &NetInput) -> bool {
        match self.entries.binary_search_by(|e| (e.tick, e.peer).cmp(&(tick, peer))) {
            Ok(_) => false,
            Err(i) => {
                self.entries.insert(i, LogEntry { tick, peer, input: input.clone() });
                true
            }
        }
    }

    /// The last applied tick anyone has input for — how long the match ran.
    pub fn last_tick(&self) -> u64 {
        self.entries.last().map(|e| e.tick).unwrap_or(0)
    }

    /// Every input for one applied tick.
    pub fn at(&self, tick: u64) -> impl Iterator<Item = &LogEntry> {
        self.entries.iter().filter(move |e| e.tick == tick)
    }

    /// Is this log complete for `tick` — does every peer in the roster have an
    /// input for it? The referee only ever simulates ticks where this is true,
    /// which is what "never guesses" means in practice.
    pub fn complete_through(&self, tick: u64) -> bool {
        (1..=tick).all(|t| {
            self.peers.iter().all(|p| self.at(t).any(|e| e.peer == *p))
        })
    }

    pub fn to_ron(&self) -> String {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
            .unwrap_or_else(|e| format!("// replay serialisation failed: {e}\n"))
    }

    /// Parse a log and refuse it unless this build would play the same match.
    pub fn from_ron(text: &str, input_map_hash: u64) -> Result<Self, LogError> {
        let log: Self = ron::from_str(text).map_err(|e| LogError::Parse(e.to_string()))?;
        if log.proto != PROTO_VERSION {
            return Err(LogError::Proto { theirs: log.proto, ours: PROTO_VERSION });
        }
        if log.input_map_hash != input_map_hash {
            return Err(LogError::InputMap);
        }
        Ok(log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> InputLog {
        InputLog::new("scenes/ring.ron", 0xABCD, 2, vec![0, 1], 0x1234)
    }

    fn input(actions: u64) -> NetInput {
        NetInput { actions, ..Default::default() }
    }

    /// The redundant window re-sends recent ticks constantly. Two recordings
    /// of one match have to be the same file or the format is useless for
    /// comparing runs — which is most of what a replay is for.
    #[test]
    fn duplicates_from_the_redundant_window_are_recorded_once() {
        let mut l = log();
        assert!(l.record(1, 5, &input(3)));
        assert!(!l.record(1, 5, &input(3)), "the same input twice is one entry");
        assert!(!l.record(1, 5, &input(9)), "and the first value is the one that counts");
        assert_eq!(l.entries.len(), 1);
        assert_eq!(l.entries[0].input.actions, 3);
    }

    /// Arrival order is a property of the network, not of the match.
    #[test]
    fn entries_sort_regardless_of_the_order_they_arrived_in() {
        let mut a = log();
        for (p, t) in [(1, 3), (0, 1), (1, 1), (0, 3), (0, 2), (1, 2)] {
            a.record(p, t, &input(t));
        }
        let mut b = log();
        for (p, t) in [(0, 1), (1, 1), (0, 2), (1, 2), (0, 3), (1, 3)] {
            b.record(p, t, &input(t));
        }
        assert_eq!(a, b, "two recordings of one match must be the same file");
        assert_eq!(a.last_tick(), 3);
    }

    /// The referee simulates only ticks it holds every peer's input for. That
    /// is the whole difference between it and a player's copy.
    #[test]
    fn completeness_is_per_tick_and_needs_every_peer() {
        let mut l = log();
        l.record(0, 1, &input(1));
        assert!(!l.complete_through(1), "one of two peers is not a confirmed tick");
        l.record(1, 1, &input(1));
        assert!(l.complete_through(1));
        l.record(0, 2, &input(1));
        assert!(!l.complete_through(2), "a gap at tick 2 is still a gap");
    }

    #[test]
    fn a_log_round_trips_through_ron() {
        let mut l = log();
        l.record(0, 1, &input(7));
        l.record(1, 1, &input(0));
        let back = InputLog::from_ron(&l.to_ron(), 0x1234).expect("round trip");
        assert_eq!(back, l);
    }

    /// Both refusals exist because the failure they prevent is SILENT: the
    /// replay would run, and play a different match.
    #[test]
    fn a_log_from_another_build_or_another_input_map_is_refused() {
        let mut l = log();
        l.record(0, 1, &input(1));
        let text = l.to_ron();

        let wrong_map = InputLog::from_ron(&text, 0x9999);
        assert_eq!(wrong_map.unwrap_err(), LogError::InputMap);

        let stale = text.replace(
            &format!("proto: {PROTO_VERSION}"),
            &format!("proto: {}", PROTO_VERSION - 1),
        );
        match InputLog::from_ron(&stale, 0x1234) {
            Err(LogError::Proto { theirs, ours }) => {
                assert_eq!((theirs, ours), (PROTO_VERSION - 1, PROTO_VERSION));
            }
            other => panic!("a log from another protocol must be refused, got {other:?}"),
        }
    }
}
