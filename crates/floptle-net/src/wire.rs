//! The wire vocabulary (`docs/netcode-design.md` §5.1), postcard-encoded.
//! Control (hello/spawn/rpc/events) rides [`Channel::Reliable`]; snapshots ride
//! [`Channel::UnreliableSequenced`] — only the newest matters, loss is healed
//! by periodic keyframes (full-state snapshots), not resends.
//!
//! v1 deliberately sends full values for CHANGED entities (dirty-flag
//! detection) rather than baseline-delta compression — correct first, compact
//! in phase 2e when the bandwidth profiler exists to measure it.

use serde::{Deserialize, Serialize};

use crate::value::NetValue;
use crate::PeerId;

/// Bump when the wire format changes incompatibly; mismatched peers are
/// refused at hello time instead of desyncing mysteriously later.
pub const PROTO_VERSION: u16 = 12;

/// Confirmed ticks between rollback state checksums (§6) — twice a second at
/// 60 Hz. Often enough that a desync is caught within a exchange or two, rare
/// enough that hashing the state ring is free.
pub const CHECKSUM_EVERY: u64 = 30;

/// One controller layer's playback in a snapshot, quantized for the wire:
/// state index (`u16::MAX` = the layer is stopped/released), clip time in
/// 10 ms units (655 s max — clamped), blend weight in 1/255ths. Both peers
/// load the same controller asset, so an index + a time reproduce the whole
/// pose locally — no bones, no strings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnimLayerWire {
    pub state: u16,
    pub t10: u16,
    pub weight: u8,
}

impl AnimLayerWire {
    pub const STOPPED: u16 = u16::MAX;

    pub fn quantize(state: Option<u16>, t: f32, weight: f32) -> Self {
        Self {
            state: state.unwrap_or(Self::STOPPED),
            t10: ((t.max(0.0) * 100.0).round() as u32).min(u16::MAX as u32) as u16,
            weight: (weight.clamp(0.0, 1.0) * 255.0).round() as u8,
        }
    }

    pub fn state_opt(self) -> Option<u16> {
        (self.state != Self::STOPPED).then_some(self.state)
    }

    pub fn t_secs(self) -> f32 {
        self.t10 as f32 / 100.0
    }

    pub fn weight_f(self) -> f32 {
        self.weight as f32 / 255.0
    }
}

/// One animator's state in a snapshot: the controller-wide speed (signed
/// 1/256ths — covers reverse playback) + its layers. `sub` addresses WHICH
/// animator under the networked node: 0 = the node itself, N = the Nth
/// animator-carrying descendant in the deterministic subtree walk — the
/// standard avatar is a Networked capsule whose CHILD Model carries the
/// controller. Sent only on CHANGE (a transition, a weight/speed edit, or
/// unpredictable time — a looping clip's time is predicted, not re-sent),
/// plus keyframes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnimEntry {
    pub id: u64, // NetId
    pub sub: u8,
    pub speed: i16,
    pub layers: Vec<AnimLayerWire>,
}

impl AnimEntry {
    pub fn speed_f(&self) -> f32 {
        self.speed as f32 / 256.0
    }

    pub fn quantize_speed(s: f32) -> i16 {
        (s.clamp(-127.0, 127.0) * 256.0).round() as i16
    }
}

/// One replicated entity's transform state in a snapshot. Position is absolute
/// world f64 (floating-origin safe); rotation a quaternion; velocity/grounded
/// present only when the node syncs physics (prediction needs both).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapEntry {
    pub id: u64, // NetId
    pub pos: [f64; 3],
    pub rot: [f32; 4],
    pub vel: Option<[f32; 3]>,
    pub grounded: Option<bool>,
}

/// A serializable per-tick input snapshot — what a client's `fixedUpdate` saw,
/// shipped to the server so the SAME controller script re-runs there with the
/// SAME input (`docs/netcode-design.md` §6, the one-script model).
///
/// This carries **resolved actions**, not raw keys: bitmasks and axis values,
/// which are both device-agnostic and fixed-size, so encoding is inherently
/// deterministic (no set iteration order to sort away).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetInput {
    /// Actions held this tick, as a bitmask indexed by the action's position in
    /// the project's `input.ron`.
    ///
    /// **Actions, not keys.** A pad player and a keyboard player who both press
    /// "Jump" produce the identical command, so one controller script replays
    /// the same on client, server, and rollback. It is also far smaller than
    /// shipping key-name strings every tick.
    ///
    /// The cost of indexing by position: both sides must agree on the map's
    /// ORDER, which is what [`Msg::Hello`]'s `input_map` hash enforces.
    pub actions: u64,
    /// Actions whose down-edge landed on this tick.
    pub just_pressed: u64,
    /// Actions whose up-edge landed on this tick.
    pub just_released: u64,
    /// 1D axis values, in `input.ron` order.
    pub axes1: Vec<f32>,
    /// 2D axis values, in `input.ron` order.
    pub axes2: Vec<(f32, f32)>,
    /// The owner's view direction — active-camera (yaw, pitch) at the tick.
    /// Camera-relative controllers read it via `input.aimYaw()` so movement is
    /// IDENTICAL on client, server, and replay (a local camera node can't be).
    pub aim: Option<[f32; 2]>,
}

/// One tick's input command (client → server).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputCmd {
    pub tick: u64,
    pub input: NetInput,
}

/// Changed `synced` script vars for one replicated entity: per script kind, the
/// vars that changed since the last send.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncedEntry {
    pub id: u64, // NetId
    pub script: String,
    pub vars: Vec<(String, NetValue)>,
}

/// Everything that crosses the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Msg {
    /// Client → server, first message on connect.
    ///
    /// `input_map` is [`floptle_input::InputMap::hash`] — a fingerprint of the
    /// action map's SHAPE (its ordered names). Input commands index actions by
    /// position, so two peers running differently-ordered maps would decode
    /// each other's input as the wrong actions and desync with no error
    /// anywhere. Refusing the connection is the only safe answer; a player's
    /// personal rebinds deliberately don't affect the hash.
    Hello { proto: u16, input_map: u64 },
    /// Server → client: accepted; your peer id, the current tick, the snapshot
    /// cadence (ticks between snapshots), the CURRENT scene (project-root-
    /// relative path + its epoch) — a late joiner lands in the scene the
    /// session is actually in, not whatever it had open — and the session's
    /// fixed rollback input delay.
    ///
    /// `input_delay` is host-set and identical on every peer, because mismatched
    /// delay is mismatched simulation (`docs/rollback-netcode-design.md` §2.2).
    /// It is carried even for a non-rollback session so a client never has to
    /// ask; a session with no `Rollback` nodes simply never applies it.
    Welcome {
        peer: PeerId,
        tick: u64,
        snapshot_every: u8,
        scene: String,
        epoch: u8,
        input_delay: u8,
    },
    /// Server → client: refused (wrong proto / full).
    Refused { reason: String },
    /// Server → clients (reliable): the session switched scenes. Clients load
    /// `scene` from their local project, re-register NetIds against it, and
    /// drop any old-epoch state still in flight.
    Scene { epoch: u8, scene: String },
    /// Server → clients: a runtime-spawned replicated node (RON `NodeDoc`).
    Spawn { epoch: u8, id: u64, node_ron: String, owner: Option<PeerId> },
    /// Server → clients: a replicated node despawned.
    Despawn { epoch: u8, id: u64 },
    /// Server → clients, at the snapshot cadence: changed transforms + synced
    /// vars + changed animator states. `keyframe` marks a periodic full-state
    /// snapshot (loss healing). `epoch` is the scene generation: NetIds only
    /// mean anything within one scene, so a stale snapshot racing a scene
    /// switch must be dropped, not applied to same-numbered strangers.
    Snapshot {
        epoch: u8,
        tick: u64,
        keyframe: bool,
        entries: Vec<SnapEntry>,
        synced: Vec<SyncedEntry>,
        anims: Vec<AnimEntry>,
    },
    /// Client → server, every tick: the last few ticks' inputs (redundant
    /// window, so one lost packet doesn't lose a tick's input).
    ///
    /// `confirmed` is the sender's ROLLBACK frontier — the newest applied tick
    /// for which it holds every peer's real input. Zero, and meaningless, in a
    /// non-rollback session.
    ///
    /// It exists because the host cannot otherwise know when it is safe to stop
    /// re-sending a tick. Its own frontier says "I have everyone's input for
    /// T", which is a different claim from "everyone HAS everyone's input for
    /// T" — and dropping on the former is what let a single lost datagram
    /// deadlock a match permanently (floptle/0039).
    Input { entries: Vec<InputCmd>, confirmed: u64 },
    /// Either direction: a named remote call. `sender` is stamped by the
    /// SERVER when relaying/receiving (clients can't spoof it). `tick` is the
    /// sender's PERCEIVED server tick (`{withInput = true}`, client → server
    /// only): the newest snapshot tick the client had applied when it fired —
    /// what lag compensation rewinds to (`docs/netcode-design.md` §7).
    Rpc { name: String, args: NetValue, sender: PeerId, tick: Option<u64> },
    /// Server → clients: another player joined/left (for `net.on` events).
    PeerJoined { peer: PeerId },
    PeerLeft { peer: PeerId },
    /// Either direction: clean goodbye.
    Bye,
    /// Server → clients: this session simulates by ROLLBACK from now, with this
    /// peer→slot assignment (`peers[n]` plays slot `n`; the host is always slot
    /// 0) and this fixed input delay.
    ///
    /// It is also the **tick origin**: receiving it starts every peer's rollback
    /// clock at 0, so tick N means the same instant on every machine. That is
    /// what lets the wire carry bare applied-tick numbers with no stamp
    /// translation — and it is why v1 does not support joining a rollback match
    /// already in progress. Spectators and late joiners need the input log plus
    /// a keyframe, which §5 files as future work.
    ///
    /// `seed` is the match's RNG seed, chosen once by the host: `net.random()`
    /// draws from (seed, tick, draw index), so every peer rolls the same
    /// numbers and a re-simulated tick rolls them again (§3). An unseeded
    /// `rng()` in a rollback sim is poison — two peers draw differently and the
    /// match quietly forks — so the engine hands out the correct thing rather
    /// than only documenting it.
    ///
    /// Re-sent whenever the roster changes, which restarts the match clock.
    RollbackStart { peers: Vec<PeerId>, input_delay: u8, seed: u64 },
    /// Host → clients, every tick: a redundant window of EVERY peer's recent
    /// APPLIED-tick inputs, so one lost packet costs nothing.
    ///
    /// The host is the arbiter and the fan-out point: peers send it their own
    /// inputs and it echoes everyone's to everyone. That also means the host
    /// holds the session's input log, which is what makes match replays, the
    /// referee and (later) spectators nearly free (§5).
    ///
    /// Ordered **oldest first**, and built from every peer's ring separately,
    /// so the tick a starved peer is waiting for is always in the packet and no
    /// peer's traffic can crowd out another's. Both were true only by accident
    /// before floptle/0039, and stopped being true the moment anyone stalled.
    Inputs { entries: Vec<(PeerId, InputCmd)> },
    /// Any peer → host → all: the state checksum for a confirmed tick (§6).
    ///
    /// Mandatory, not optional. Determinism across builds and platforms is
    /// *expected*, not proven, and a rollback session without checksums doesn't
    /// fail loudly — it plays a subtly different match on each screen until
    /// someone notices the health bars disagree.
    StateHash { tick: u64, hash: u64 },
    /// Host → clients: the checksums for `tick` did not agree. Every peer
    /// surfaces this loudly — Console error, red in the Hub panel, and
    /// `net.on("desync")` so the game can end the match honestly rather than
    /// play out two different fights.
    Desync { tick: u64 },
    /// Server → one client, periodically: input-timing feedback. `margin` is
    /// the smoothed number of ticks of that client's input still buffered
    /// ahead when the server consumes one (negative = arriving LATE,
    /// repeat-last in use — mispredictions on the owner); `late` is the
    /// running repeat-last count for that peer. The client auto-tunes its
    /// input lead from this, so clock hitches and drift self-heal instead of
    /// turning into permanent correction storms (`docs/netcode-design.md` §6).
    InputAck { margin: i32, late: u64 },
    /// Round-trip probe. Whoever receives one replies with a [`Msg::Pong`]
    /// carrying the same `id`, immediately — the point is to measure the link,
    /// so anything the responder does first is measurement error.
    ///
    /// Application level rather than transport level on purpose: through a
    /// relay the transport can only see its own leg (host↔relay), and the
    /// number a game actually needs is host↔player. This measures that, and it
    /// measures it the same way over every transport there will ever be.
    Ping { id: u32 },
    Pong { id: u32 },
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("wire messages always encode")
    }

    pub fn decode(bytes: &[u8]) -> Option<Msg> {
        postcard::from_bytes(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let m = Msg::Snapshot {
            epoch: 3,
            tick: 424242,
            keyframe: true,
            entries: vec![SnapEntry {
                id: 7,
                pos: [1.0e6, 2.5, -3.0],
                rot: [0.0, std::f32::consts::FRAC_1_SQRT_2, 0.0, std::f32::consts::FRAC_1_SQRT_2],
                vel: Some([0.0, -9.8, 0.0]),
                grounded: Some(true),
            }],
            synced: vec![SyncedEntry {
                id: 7,
                script: "combat".into(),
                vars: vec![("parrying".into(), NetValue::Bool(true))],
            }],
            anims: vec![AnimEntry {
                id: 7,
                sub: 1,
                speed: AnimEntry::quantize_speed(1.0),
                layers: vec![
                    AnimLayerWire::quantize(Some(3), 1.25, 1.0),
                    AnimLayerWire::quantize(None, 0.0, 0.5),
                ],
            }],
        };
        assert_eq!(Msg::decode(&m.encode()), Some(m));
        assert_eq!(Msg::decode(b"garbage\xff\xff"), None);
    }

    /// The rollback additions have to survive the wire too — and `Inputs` is the
    /// one message a fighter sends sixty times a second, so a decode failure
    /// there is the whole match.
    #[test]
    fn rollback_messages_round_trip() {
        for m in [
            Msg::Welcome {
                peer: 3,
                tick: 900,
                snapshot_every: 2,
                scene: "scenes/ring.ron".into(),
                epoch: 1,
                input_delay: 2,
            },
            Msg::RollbackStart { peers: vec![0, 3], input_delay: 2, seed: 0x1234_5678_9abc_def0 },
            Msg::Inputs {
                entries: vec![
                    (0, InputCmd { tick: 120, input: NetInput { actions: 0b101, ..Default::default() } }),
                    (
                        3,
                        InputCmd {
                            tick: 120,
                            input: NetInput {
                                actions: 0b10,
                                just_pressed: 0b10,
                                axes2: vec![(-1.0, 0.0)],
                                aim: Some([0.5, -0.25]),
                                ..Default::default()
                            },
                        },
                    ),
                ],
            },
            Msg::StateHash { tick: 90, hash: 0xdead_beef_cafe_f00d },
            Msg::Desync { tick: 90 },
        ] {
            assert_eq!(Msg::decode(&m.encode()), Some(m.clone()), "{m:?}");
        }
    }

    #[test]
    fn anim_wire_quantization_round_trips() {
        let l = AnimLayerWire::quantize(Some(12), 3.774, 0.5);
        assert_eq!(l.state_opt(), Some(12));
        assert!((l.t_secs() - 3.77).abs() < 0.006, "10 ms resolution");
        assert!((l.weight_f() - 0.5).abs() < 0.003);
        let stopped = AnimLayerWire::quantize(None, 99.0, 1.0);
        assert_eq!(stopped.state_opt(), None);
        // Speed covers reverse playback and survives the fixed-point trip.
        let s = AnimEntry { id: 1, sub: 0, speed: AnimEntry::quantize_speed(-1.5), layers: vec![] };
        assert!((s.speed_f() + 1.5).abs() < 1.0 / 256.0);
        // Times beyond the u16 range clamp instead of wrapping to nonsense.
        let long = AnimLayerWire::quantize(Some(0), 1e6, 1.0);
        assert_eq!(long.t10, u16::MAX);
    }
}
