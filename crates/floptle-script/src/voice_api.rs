//! The Lua `voice.*` API — proximity voice chat (`floptle/0180`).
//!
//! Follows the same queue-drain shape as `net.*`: calls push [`VoiceCmd`]s the
//! editor drains each tick, and live state (devices, mic level, who is
//! speaking) is mirrored IN through [`VoiceState`].
//!
//! ## The shape a game actually writes
//!
//! ```lua
//! -- settings screen
//! for _, d in ipairs(voice.devices()) do ... end
//! voice.setDevice(name)
//! meter.width = voice.level() * 120
//!
//! -- input
//! voice.setTransmit(input.action("Talk"))          -- push-to-talk
//!
//! -- match script, on every client, once per peer
//! voice.attach(peer, avatarNode, { mode = "Spatial", falloff = "Inverse",
//!                                  minDistance = 2, maxDistance = 22,
//!                                  track = "Voice" })
//! voice.source(peer):setTrack(isMonster and "Voice Monster" or "Voice")
//!
//! -- server
//! voice.setForward(deadPeer, deadPeers)            -- the dead talk to the dead
//! ```
//!
//! ## Why there is no per-effect API here
//!
//! A remote speaker plays through a mixer track like every other sound, so the
//! game authors `Voice`, `Voice Monster` (PitchShift, Distortion, Reverb…) and
//! `Voice Dead` in `project.ron` and moves a peer between them with
//! `:setTrack`. Turning the killer into a monster needs no new audio API at
//! all — which is the point of a voice stream being an ordinary voice.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use crate::{LogLevel, ScriptLog};

/// Every key `voice.attach` / `voice.source(...)` options tables read
/// (`floptle/0082` — a misspelled option must not silently do nothing).
pub(crate) const ATTACH_KEYS: &[&str] =
    &["mode", "falloff", "minDistance", "maxDistance", "volume", "track"];

/// A queued voice command, drained by the editor each tick.
#[derive(Clone, Debug, PartialEq)]
pub enum VoiceCmd {
    /// `voice.setDevice(name)` — `None` picks the system default.
    SetDevice { name: Option<String> },
    /// `voice.setTransmit(bool)` — open or close the microphone.
    SetTransmit { on: bool },
    /// `voice.attach(peer, node, opts)` — a remote speaker's voice comes out of
    /// that node and follows it.
    Attach { peer: u64, eid: u32, opts: VoiceOpts },
    /// `voice.detach(peer)` — the voice goes back to being unpositioned.
    Detach { peer: u64 },
    /// Retune a live source (`voice.source(peer):setTrack(...)` and friends).
    Params { peer: u64, opts: VoiceOpts },
    /// `voice.mute(peer, bool)` — a LOCAL mute. Never leaves this machine: it
    /// is one player's choice not to listen, not a rule about who may speak.
    Mute { peer: u64, muted: bool },
    /// `voice.setForward(peer, { peers })` — SERVER: who may hear `peer`.
    /// `None` = everyone.
    SetForward { peer: u64, to: Option<Vec<u64>> },
    /// `voice.sidetone(bool)` — hear your own microphone. Off by default.
    Sidetone { on: bool },
}

/// The tunables a voice source carries — the same knob set `audio.play` takes,
/// because a remote speaker is an ordinary spatial sound.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoiceOpts {
    pub mode: Option<String>,
    pub falloff: Option<String>,
    pub min_distance: Option<f32>,
    pub max_distance: Option<f32>,
    pub volume: Option<f32>,
    pub track: Option<String>,
}

impl VoiceOpts {
    fn read(t: &Table) -> mlua::Result<Self> {
        crate::opts::check_keys(t, ATTACH_KEYS, "voice.attach")?;
        Ok(Self {
            mode: t.get::<Option<String>>("mode").ok().flatten(),
            falloff: t.get::<Option<String>>("falloff").ok().flatten(),
            min_distance: t.get::<Option<f32>>("minDistance").ok().flatten(),
            max_distance: t.get::<Option<f32>>("maxDistance").ok().flatten(),
            volume: t.get::<Option<f32>>("volume").ok().flatten(),
            track: t.get::<Option<String>>("track").ok().flatten(),
        })
    }
}

/// Live voice state, fed by the editor each tick and read by Lua.
#[derive(Clone, Debug, Default)]
pub struct VoiceState {
    /// Input device names. Empty on a machine with no microphone.
    pub devices: Vec<String>,
    /// The device currently open.
    pub device: Option<String>,
    /// Local microphone RMS, 0..1 — live whether or not transmit is on, so a
    /// settings screen can prove the mic works without joining a lobby.
    pub level: f32,
    /// Is the microphone open right now?
    pub transmitting: bool,
    /// Peers whose frames are arriving above the gate — for a HUD indicator.
    pub speaking: Vec<u64>,
    /// Peers this machine has muted locally.
    pub muted: Vec<u64>,
    /// Peers with a live incoming stream (whether or not they are speaking).
    pub sources: Vec<u64>,
}

/// The shared control block between Lua and the driver.
pub(crate) struct SharedVoice {
    pub cmds: Rc<RefCell<Vec<VoiceCmd>>>,
    pub state: Rc<RefCell<VoiceState>>,
    pub logs: Rc<RefCell<Vec<ScriptLog>>>,
    /// Is this endpoint the server? `voice.setForward` is server-only, and
    /// saying so beats doing nothing quietly.
    pub is_server: Rc<std::cell::Cell<bool>>,
}

impl SharedVoice {
    pub fn new(logs: Rc<RefCell<Vec<ScriptLog>>>) -> Self {
        Self {
            cmds: Rc::new(RefCell::new(Vec::new())),
            state: Rc::new(RefCell::new(VoiceState::default())),
            logs,
            is_server: Rc::new(std::cell::Cell::new(false)),
        }
    }

}

/// Build the `voice` global.
pub(crate) fn install_voice_api(lua: &Lua, voice: &SharedVoice) -> mlua::Result<()> {
    let t = lua.create_table()?;

    // --- the microphone ---------------------------------------------------
    {
        let v = voice.state.clone();
        t.set(
            "devices",
            lua.create_function(move |lua, ()| {
                // An empty list is the honest answer on a machine with no
                // microphone, and a settings screen can say "no microphone
                // found" rather than showing an empty dropdown for no reason.
                lua.create_sequence_from(v.borrow().devices.clone())
            })?,
        )?;
    }
    {
        let v = voice.state.clone();
        t.set("device", lua.create_function(move |_, ()| Ok(v.borrow().device.clone()))?)?;
    }
    {
        let sv = voice.cmds.clone();
        t.set(
            "setDevice",
            lua.create_function(move |_, name: Option<String>| {
                sv.borrow_mut().push(VoiceCmd::SetDevice { name });
                Ok(())
            })?,
        )?;
    }
    {
        let sv = voice.cmds.clone();
        t.set(
            "setTransmit",
            lua.create_function(move |_, on: bool| {
                sv.borrow_mut().push(VoiceCmd::SetTransmit { on });
                Ok(())
            })?,
        )?;
    }
    {
        let v = voice.state.clone();
        t.set("level", lua.create_function(move |_, ()| Ok(v.borrow().level))?)?;
    }
    {
        let v = voice.state.clone();
        t.set("transmitting", lua.create_function(move |_, ()| Ok(v.borrow().transmitting))?)?;
    }
    {
        let sv = voice.cmds.clone();
        t.set(
            "sidetone",
            lua.create_function(move |_, on: bool| {
                sv.borrow_mut().push(VoiceCmd::Sidetone { on });
                Ok(())
            })?,
        )?;
    }

    // --- remote speakers --------------------------------------------------
    {
        let v = voice.state.clone();
        t.set(
            "speaking",
            lua.create_function(move |_, peer: u64| Ok(v.borrow().speaking.contains(&peer)))?,
        )?;
    }
    {
        let cmds = voice.cmds.clone();
        let state = voice.state.clone();
        t.set(
            "mute",
            lua.create_function(move |_, (peer, muted): (u64, Option<bool>)| {
                let muted = muted.unwrap_or(true);
                // Mirrored immediately as well as queued: a HUD that reads
                // `voice.muted(peer)` on the same frame it muted them should
                // not show the old answer for a tick.
                let mut st = state.borrow_mut();
                st.muted.retain(|p| *p != peer);
                if muted {
                    st.muted.push(peer);
                }
                drop(st);
                cmds.borrow_mut().push(VoiceCmd::Mute { peer, muted });
                Ok(())
            })?,
        )?;
    }
    {
        let v = voice.state.clone();
        t.set(
            "muted",
            lua.create_function(move |_, peer: u64| Ok(v.borrow().muted.contains(&peer)))?,
        )?;
    }
    {
        let cmds = voice.cmds.clone();
        t.set(
            "attach",
            lua.create_function(move |_, (peer, node, opts): (u64, Table, Option<Table>)| {
                let opts = match &opts {
                    Some(o) => VoiceOpts::read(o)?,
                    None => VoiceOpts::default(),
                };
                if let Ok(eid) = node.raw_get::<u32>("__id") {
                    cmds.borrow_mut().push(VoiceCmd::Attach { peer, eid, opts });
                }
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = voice.cmds.clone();
        t.set(
            "detach",
            lua.create_function(move |_, peer: u64| {
                cmds.borrow_mut().push(VoiceCmd::Detach { peer });
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = voice.cmds.clone();
        let state = voice.state.clone();
        t.set(
            "source",
            lua.create_function(move |lua, peer: u64| {
                // A handle shaped like the one `audio.play` returns, because a
                // remote speaker IS an ordinary voice — the same `:setTrack`,
                // `:setVolume`, `:setPosition` a game already knows.
                make_source(lua, peer, &cmds, &state)
            })?,
        )?;
    }

    // --- the server's say -------------------------------------------------
    {
        let cmds = voice.cmds.clone();
        let is_server = voice.is_server.clone();
        let logs = voice.logs.clone();
        t.set(
            "setForward",
            lua.create_function(move |_, (peer, to): (u64, Value)| {
                if !is_server.get() {
                    logs.borrow_mut().push(ScriptLog {
                        level: LogLevel::Warn,
                        msg: "voice.setForward: only the server decides who hears whom \
                                  — ignored. A client turning a stream down is a volume \
                                  slider, not a rule"
                            .into(),
                        source: None,
                    });
                    return Ok(());
                }
                let to = match to {
                    Value::Nil => None,
                    Value::Table(t) => {
                        Some(t.sequence_values::<u64>().filter_map(|p| p.ok()).collect())
                    }
                    other => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "voice.setForward(peer, peers): peers must be a list of peer ids \
                             or nil for everyone, got {}",
                            other.type_name()
                        )))
                    }
                };
                cmds.borrow_mut().push(VoiceCmd::SetForward { peer, to });
                Ok(())
            })?,
        )?;
    }

    lua.globals().set("voice", t)
}

/// The handle `voice.source(peer)` returns.
fn make_source(
    lua: &Lua,
    peer: u64,
    cmds: &Rc<RefCell<Vec<VoiceCmd>>>,
    state: &Rc<RefCell<VoiceState>>,
) -> mlua::Result<Table> {
    let h = lua.create_table()?;
    h.set("peer", peer)?;
    // Whether the stream exists at all, so a game can tell "quiet" from "not
    // in this session".
    h.set("live", state.borrow().sources.contains(&peer))?;

    let setter = |field: &'static str| {
        let cmds = cmds.clone();
        move |_: &Lua, (_this, value): (Table, Value)| {
            let mut opts = VoiceOpts::default();
            match field {
                "track" => opts.track = value.as_string().map(|s| s.to_string_lossy().to_string()),
                "volume" => opts.volume = value.as_f32(),
                "minDistance" => opts.min_distance = value.as_f32(),
                "maxDistance" => opts.max_distance = value.as_f32(),
                "mode" => opts.mode = value.as_string().map(|s| s.to_string_lossy().to_string()),
                "falloff" => {
                    opts.falloff = value.as_string().map(|s| s.to_string_lossy().to_string())
                }
                _ => {}
            }
            cmds.borrow_mut().push(VoiceCmd::Params { peer, opts });
            Ok(())
        }
    };
    h.set("setTrack", lua.create_function(setter("track"))?)?;
    h.set("setVolume", lua.create_function(setter("volume"))?)?;
    h.set("setMinDistance", lua.create_function(setter("minDistance"))?)?;
    h.set("setMaxDistance", lua.create_function(setter("maxDistance"))?)?;
    h.set("setMode", lua.create_function(setter("mode"))?)?;
    h.set("setFalloff", lua.create_function(setter("falloff"))?)?;
    {
        let cmds = cmds.clone();
        h.set(
            "setPosition",
            lua.create_function(move |_, (_this, node): (Table, Table)| {
                // Position comes from a NODE, not three numbers: a voice that
                // has to be moved by hand every frame is one that will be
                // forgotten in some code path and left across the map.
                if let Ok(eid) = node.raw_get::<u32>("__id") {
                    cmds.borrow_mut()
                        .push(VoiceCmd::Attach { peer, eid, opts: VoiceOpts::default() });
                }
                Ok(())
            })?,
        )?;
    }
    Ok(h)
}
