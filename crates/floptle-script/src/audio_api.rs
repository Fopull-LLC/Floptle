//! The Lua `audio` API: fire-and-forget one-shots (`audio.play`), sound
//! handles, mixer-track control, and the `node:sound()` handle for a node's
//! `AudioSource` component.
//!
//! Same decoupling as anim/vfx: every call queues an [`AudioCmd`] the editor
//! drains after `run` and applies to the real audio engine; state reads come
//! from the [`AudioInfo`] mirror the editor feeds before `run`.

use std::cell::RefCell;
use std::rc::Rc;

use floptle_audio::{EndBehavior, Falloff, PlayParams, SpatialMode};
use mlua::{Lua, Table, Value};

use crate::{AudioAt, AudioCmd, AudioInfo};

/// The shared bridges the `audio` closures capture.
pub(crate) struct AudioBridges {
    pub commands: Rc<RefCell<Vec<AudioCmd>>>,
    pub info: Rc<RefCell<AudioInfo>>,
    /// Next script-side sound handle id (monotonic within a play session).
    pub next_handle: Rc<RefCell<u32>>,
}

/// Read a play-options table into `PlayParams`. Unknown keys are ignored;
/// enum strings are case-insensitive (`mode = "spatial"`, `falloff =
/// "linear"`, `endBehavior = "destroy"`, `loop = true`).
fn parse_params(opts: Option<&Table>) -> PlayParams {
    let mut p = PlayParams::default();
    let Some(t) = opts else { return p };
    let num = |key: &str| t.raw_get::<f64>(key).ok();
    if let Some(v) = num("volume") {
        p.volume = v as f32;
    }
    if let Some(v) = num("pitch") {
        p.pitch = v as f32;
    }
    if let Some(v) = num("pan") {
        p.pan = v as f32;
    }
    if let Some(v) = num("minDistance") {
        p.min_distance = v as f32;
    }
    if let Some(v) = num("maxDistance") {
        p.max_distance = v as f32;
    }
    if let Ok(s) = t.raw_get::<String>("mode")
        && let Some(m) = SpatialMode::parse(&s)
    {
        p.mode = m;
    }
    if let Ok(s) = t.raw_get::<String>("falloff")
        && let Some(f) = Falloff::parse(&s)
    {
        p.falloff = f;
    }
    if let Ok(s) = t.raw_get::<String>("track") {
        p.track = s;
    }
    if let Ok(s) = t.raw_get::<String>("endBehavior")
        && let Some(e) = EndBehavior::parse(&s)
    {
        p.end = e;
    }
    // `loop = true` is the friendlier spelling of endBehavior = "Loop".
    if let Ok(true) = t.raw_get::<bool>("loop") {
        p.end = EndBehavior::Loop;
    }
    p
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => Some(*n),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

pub(crate) fn install_audio_api(lua: &Lua, b: &AudioBridges) -> mlua::Result<()> {
    // ---- the sound handle (returned by audio.play) --------------------------
    let methods = lua.create_table()?;
    for (name, make) in [
        ("stop", 0u8),
        ("pause", 1),
        ("resume", 2),
    ] {
        let cmds = b.commands.clone();
        methods.set(
            name,
            lua.create_function(move |_, this: Table| {
                let id: u32 = this.raw_get("__sound")?;
                cmds.borrow_mut().push(match make {
                    0 => AudioCmd::Stop { handle: id },
                    1 => AudioCmd::Pause { handle: id, paused: true },
                    _ => AudioCmd::Pause { handle: id, paused: false },
                });
                Ok(())
            })?,
        )?;
    }
    for field in ["volume", "pitch", "pan"] {
        let cmds = b.commands.clone();
        // setVolume / setPitch / setPan
        let name = format!("set{}{}", field[..1].to_uppercase(), &field[1..]);
        methods.set(
            name.as_str(),
            lua.create_function(move |_, (this, v): (Table, f64)| {
                let id: u32 = this.raw_get("__sound")?;
                cmds.borrow_mut().push(AudioCmd::SetParam {
                    handle: id,
                    field: field.to_string(),
                    value: v,
                });
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = b.commands.clone();
        methods.set(
            "setTrack",
            lua.create_function(move |_, (this, track): (Table, String)| {
                let id: u32 = this.raw_get("__sound")?;
                cmds.borrow_mut().push(AudioCmd::SetTrack { handle: id, track });
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = b.commands.clone();
        methods.set(
            "setPosition",
            lua.create_function(move |_, (this, x, y, z): (Table, f64, f64, f64)| {
                let id: u32 = this.raw_get("__sound")?;
                cmds.borrow_mut().push(AudioCmd::Move { handle: id, pos: [x, y, z] });
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = b.commands.clone();
        methods.set(
            "seek",
            lua.create_function(move |_, (this, secs): (Table, f64)| {
                let id: u32 = this.raw_get("__sound")?;
                cmds.borrow_mut().push(AudioCmd::Seek { handle: id, secs });
                Ok(())
            })?,
        )?;
    }
    {
        let inf = b.info.clone();
        methods.set(
            "isPlaying",
            lua.create_function(move |_, this: Table| {
                let id: u32 = this.raw_get("__sound")?;
                Ok(inf.borrow().sounds.get(&id).map(|s| s.playing).unwrap_or(false))
            })?,
        )?;
    }
    {
        let inf = b.info.clone();
        methods.set(
            "position",
            lua.create_function(move |_, this: Table| {
                let id: u32 = this.raw_get("__sound")?;
                Ok(inf.borrow().sounds.get(&id).map(|s| s.position).unwrap_or(0.0))
            })?,
        )?;
    }
    let sound_mt = lua.create_table()?;
    sound_mt.set("__index", methods)?;
    lua.set_named_registry_value("floptle_sound_mt", sound_mt)?;

    // ---- the mixer track handle (audio.track("Music")) ----------------------
    let tmethods = lua.create_table()?;
    {
        let cmds = b.commands.clone();
        tmethods.set(
            "setVolume",
            lua.create_function(move |_, (this, db): (Table, f64)| {
                let track: String = this.raw_get("__track")?;
                cmds.borrow_mut().push(AudioCmd::TrackVolume { track, db });
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = b.commands.clone();
        tmethods.set(
            "setPan",
            lua.create_function(move |_, (this, pan): (Table, f64)| {
                let track: String = this.raw_get("__track")?;
                cmds.borrow_mut().push(AudioCmd::TrackPan { track, pan });
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = b.commands.clone();
        tmethods.set(
            "setMuted",
            lua.create_function(move |_, (this, muted): (Table, bool)| {
                let track: String = this.raw_get("__track")?;
                cmds.borrow_mut().push(AudioCmd::TrackMuted { track, muted });
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = b.commands.clone();
        tmethods.set(
            "setSoloed",
            lua.create_function(move |_, (this, soloed): (Table, bool)| {
                let track: String = this.raw_get("__track")?;
                cmds.borrow_mut().push(AudioCmd::TrackSoloed { track, soloed });
                Ok(())
            })?,
        )?;
    }
    let track_mt = lua.create_table()?;
    track_mt.set("__index", tmethods)?;
    lua.set_named_registry_value("floptle_audio_track_mt", track_mt)?;

    // ---- the `audio` global --------------------------------------------------
    let audio = lua.create_table()?;
    {
        // audio.play(clip [, node | x, y, z] [, opts]) → sound handle.
        //   audio.play("audio/ding.ogg")                      -- flat (UI/music)
        //   audio.play("audio/hit.ogg", 4, 1, -2, {…})        -- at a world point
        //   audio.play("audio/engine.ogg", carNode, {loop = true}) -- follows the node
        let cmds = b.commands.clone();
        let next = b.next_handle.clone();
        audio.set(
            "play",
            lua.create_function(
                move |lua,
                      (clip, a, c1, c2, c3): (
                    String,
                    Option<Value>,
                    Option<Value>,
                    Option<Value>,
                    Option<Value>,
                )| {
                    let (at, opts) = match &a {
                        None => (AudioAt::Flat, None),
                        Some(Value::Table(t)) => {
                            if let Ok(id) = t.raw_get::<u32>("__id") {
                                let o = match &c1 {
                                    Some(Value::Table(o)) => Some(o.clone()),
                                    _ => None,
                                };
                                (AudioAt::Node(id), o)
                            } else {
                                (AudioAt::Flat, Some(t.clone()))
                            }
                        }
                        Some(v) if as_f64(v).is_some() => {
                            let x = as_f64(v).unwrap_or(0.0);
                            let y = c1.as_ref().and_then(as_f64).unwrap_or(0.0);
                            let z = c2.as_ref().and_then(as_f64).unwrap_or(0.0);
                            let o = match &c3 {
                                Some(Value::Table(o)) => Some(o.clone()),
                                _ => None,
                            };
                            (AudioAt::Pos([x, y, z]), o)
                        }
                        _ => (AudioAt::Flat, None),
                    };
                    let params = parse_params(opts.as_ref());
                    let handle = {
                        let mut n = next.borrow_mut();
                        *n += 1;
                        *n
                    };
                    cmds.borrow_mut().push(AudioCmd::Play {
                        handle,
                        clip,
                        at,
                        params: Box::new(params),
                    });
                    let t = lua.create_table()?;
                    t.raw_set("__sound", handle)?;
                    if let Ok(mt) = lua.named_registry_value::<Table>("floptle_sound_mt") {
                        t.set_metatable(Some(mt));
                    }
                    Ok(t)
                },
            )?,
        )?;
    }
    {
        let cmds = b.commands.clone();
        audio.set(
            "stopAll",
            lua.create_function(move |_, ()| {
                cmds.borrow_mut().push(AudioCmd::StopAll);
                Ok(())
            })?,
        )?;
    }
    {
        // audio.track("Music") → mixer-track handle ("Master" = the master).
        audio.set(
            "track",
            lua.create_function(move |lua, name: String| {
                let t = lua.create_table()?;
                t.raw_set("__track", name)?;
                if let Ok(mt) = lua.named_registry_value::<Table>("floptle_audio_track_mt") {
                    t.set_metatable(Some(mt));
                }
                Ok(t)
            })?,
        )?;
    }
    lua.globals().set("audio", audio)?;

    // ---- node:sound() — handle for the node's AudioSource component ---------
    let smethods = lua.create_table()?;
    for (name, which) in [("play", 0u8), ("stop", 1), ("pause", 2), ("resume", 3)] {
        let cmds = b.commands.clone();
        smethods.set(
            name,
            lua.create_function(move |_, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                cmds.borrow_mut().push(match which {
                    0 => AudioCmd::SourcePlay { ent: e },
                    1 => AudioCmd::SourceStop { ent: e },
                    2 => AudioCmd::SourcePause { ent: e, paused: true },
                    _ => AudioCmd::SourcePause { ent: e, paused: false },
                });
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = b.commands.clone();
        smethods.set(
            "setClip",
            lua.create_function(move |_, (this, clip): (Table, String)| {
                let e: u32 = this.raw_get("__id")?;
                cmds.borrow_mut().push(AudioCmd::SourceSetClip { ent: e, clip });
                Ok(())
            })?,
        )?;
    }
    {
        let cmds = b.commands.clone();
        smethods.set(
            "seek",
            lua.create_function(move |_, (this, secs): (Table, f64)| {
                let e: u32 = this.raw_get("__id")?;
                cmds.borrow_mut().push(AudioCmd::SourceSeek { ent: e, secs });
                Ok(())
            })?,
        )?;
    }
    {
        let inf = b.info.clone();
        smethods.set(
            "isPlaying",
            lua.create_function(move |_, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                Ok(inf.borrow().sources.get(&e).map(|s| s.playing).unwrap_or(false))
            })?,
        )?;
    }
    {
        let inf = b.info.clone();
        smethods.set(
            "position",
            lua.create_function(move |_, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                Ok(inf.borrow().sources.get(&e).map(|s| s.position).unwrap_or(0.0))
            })?,
        )?;
    }
    let src_mt = lua.create_table()?;
    src_mt.set("__index", smethods)?;
    lua.set_named_registry_value("floptle_sound_src_mt", src_mt)?;

    // Attach `sound()` to the node methods table (installed by the handle
    // API — must run after `install_handle_api`).
    if let Ok(node_methods) = lua.named_registry_value::<Table>("floptle_node_methods") {
        node_methods.set(
            "sound",
            lua.create_function(move |lua, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                let t = lua.create_table()?;
                t.raw_set("__id", e)?;
                if let Ok(mt) = lua.named_registry_value::<Table>("floptle_sound_src_mt") {
                    t.set_metatable(Some(mt));
                }
                Ok(t)
            })?,
        )?;
    }
    Ok(())
}
