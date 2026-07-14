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
pub const PROTO_VERSION: u16 = 6;

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

/// One replicated entity's animator state in a snapshot: the controller-wide
/// speed (signed 1/256ths — covers reverse playback) + its layers. Sent only
/// on CHANGE (a transition, a weight/speed edit, or unpredictable time — a
/// looping clip's time is predicted, not re-sent), plus keyframes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnimEntry {
    pub id: u64, // NetId
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
/// SAME input (`docs/netcode-design.md` §6, the one-script model). Key/button
/// sets are sorted Vecs so encoding is deterministic.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetInput {
    pub keys_down: Vec<String>,
    pub keys_pressed: Vec<String>,
    pub keys_released: Vec<String>,
    pub mouse: (f32, f32),
    pub mouse_delta: (f32, f32),
    pub scroll: f32,
    pub buttons_down: [bool; 3],
    pub buttons_pressed: [bool; 3],
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
    Hello { proto: u16 },
    /// Server → client: accepted; your peer id, the current tick, and the
    /// snapshot cadence (ticks between snapshots).
    Welcome { peer: PeerId, tick: u64, snapshot_every: u8 },
    /// Server → client: refused (wrong proto / full).
    Refused { reason: String },
    /// Server → clients: a runtime-spawned replicated node (RON `NodeDoc`).
    Spawn { id: u64, node_ron: String, owner: Option<PeerId> },
    /// Server → clients: a replicated node despawned.
    Despawn { id: u64 },
    /// Server → clients, at the snapshot cadence: changed transforms + synced
    /// vars + changed animator states. `keyframe` marks a periodic full-state
    /// snapshot (loss healing).
    Snapshot {
        tick: u64,
        keyframe: bool,
        entries: Vec<SnapEntry>,
        synced: Vec<SyncedEntry>,
        anims: Vec<AnimEntry>,
    },
    /// Client → server, every tick: the last few ticks' inputs (redundant
    /// window, so one lost packet doesn't lose a tick's input).
    Input { entries: Vec<InputCmd> },
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
    /// Server → one client, periodically: input-timing feedback. `margin` is
    /// the smoothed number of ticks of that client's input still buffered
    /// ahead when the server consumes one (negative = arriving LATE,
    /// repeat-last in use — mispredictions on the owner); `late` is the
    /// running repeat-last count for that peer. The client auto-tunes its
    /// input lead from this, so clock hitches and drift self-heal instead of
    /// turning into permanent correction storms (`docs/netcode-design.md` §6).
    InputAck { margin: i32, late: u64 },
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

    #[test]
    fn anim_wire_quantization_round_trips() {
        let l = AnimLayerWire::quantize(Some(12), 3.774, 0.5);
        assert_eq!(l.state_opt(), Some(12));
        assert!((l.t_secs() - 3.77).abs() < 0.006, "10 ms resolution");
        assert!((l.weight_f() - 0.5).abs() < 0.003);
        let stopped = AnimLayerWire::quantize(None, 99.0, 1.0);
        assert_eq!(stopped.state_opt(), None);
        // Speed covers reverse playback and survives the fixed-point trip.
        let s = AnimEntry { id: 1, speed: AnimEntry::quantize_speed(-1.5), layers: vec![] };
        assert!((s.speed_f() + 1.5).abs() < 1.0 / 256.0);
        // Times beyond the u16 range clamp instead of wrapping to nonsense.
        let long = AnimLayerWire::quantize(Some(0), 1e6, 1.0);
        assert_eq!(long.t10, u16::MAX);
    }
}
