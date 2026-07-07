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
pub const PROTO_VERSION: u16 = 2;

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
    /// vars. `keyframe` marks a periodic full-state snapshot (loss healing).
    Snapshot { tick: u64, keyframe: bool, entries: Vec<SnapEntry>, synced: Vec<SyncedEntry> },
    /// Client → server, every tick: the last few ticks' inputs (redundant
    /// window, so one lost packet doesn't lose a tick's input).
    Input { entries: Vec<InputCmd> },
    /// Either direction: a named remote call. `sender` is stamped by the
    /// SERVER when relaying/receiving (clients can't spoof it).
    Rpc { name: String, args: NetValue, sender: PeerId },
    /// Server → clients: another player joined/left (for `net.on` events).
    PeerJoined { peer: PeerId },
    PeerLeft { peer: PeerId },
    /// Either direction: clean goodbye.
    Bye,
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
        };
        assert_eq!(Msg::decode(&m.encode()), Some(m));
        assert_eq!(Msg::decode(b"garbage\xff\xff"), None);
    }
}
