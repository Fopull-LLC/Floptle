//! Pictures a game takes of its own world: a level's cover, a photo mode, a
//! thumbnail for a save slot.
//!
//! ```lua
//! camera.capture([cam,] { w = 640, h = 360 [, format = "png"|"jpeg", quality = 90,
//!                         ui = false, draws = false] }, function(bytes, err) end)
//! camera.captureTexture([cam,] { w, h [, ui, draws] }, function(tex, err) end)
//! ```
//!
//! `cam` is a camera node, a camera's name, or left out for the active one.
//! A camera that is not the active one is drawn off-screen, so the player's
//! view never changes. The picture is the game's own: post-processing, fog,
//! retro and render scale included. The screen UI and script `draw.line` /
//! `draw.tri` shapes are left out unless `ui` / `draws` say otherwise, because
//! a HUD or a debug line in a cover spoils it.
//!
//! `bytes` is an encoded PNG or JPEG, ready for `assets.writeBytes`, a Cloud
//! blob, or `assets.textureFromBytes`. `captureTexture` skips the encode and
//! answers a texture name like `textureFromBytes` does, released with
//! `assets.release` and let go on Stop.
//!
//! The book is kept here; the driver renders on a later frame, reads the
//! picture back without waiting on the GPU, encodes on a worker and answers.
//! A host that draws nothing (a dedicated server, `floptle run`) answers
//! `nil, "no renderer"` at once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, Lua, Table, Value};

use crate::texture_api::TextureLoads;
use crate::{LogLevel, ScriptLog};

/// The largest side a capture may ask for, in pixels. A 4K picture is 33 MB
/// of pixels before it is encoded; anything bigger is a mistake in a script.
pub const MAX_CAPTURE_SIDE: u32 = 4096;
/// Captures waiting for the GPU at once. Each costs a render of the scene, so
/// a script that asks for one a frame would halve the frame rate; past this it
/// is told so rather than queued without end.
pub const MAX_CAPTURES_IN_FLIGHT: usize = 4;

/// The answer on a host with no GPU.
pub const NO_RENDERER: &str = "no renderer";

/// Which camera a capture looks through.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureCamera {
    /// The scene's active camera.
    Active,
    /// A node, by entity index.
    Node(u32),
    /// A camera, by node name.
    Named(String),
}

/// What the driver hands back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaptureFormat {
    Png,
    /// Quality 1–100.
    Jpeg(u8),
    /// No encode: the pixels become a texture named `img:<id>`, answered
    /// through the texture book so `assets.release` and Stop cover it.
    Texture,
}

/// One picture a script asked for.
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    pub id: u64,
    pub camera: CaptureCamera,
    pub w: u32,
    pub h: u32,
    pub format: CaptureFormat,
    /// The screen UI and world-space canvases.
    pub ui: bool,
    /// Script `draw.line` / `draw.tri` shapes.
    pub draws: bool,
    /// For [`CaptureFormat::Texture`]: the name to register.
    pub texture: Option<String>,
}

/// What scripts asked for and who is waiting.
#[derive(Default)]
pub(crate) struct Captures {
    next: u64,
    requests: Vec<CaptureRequest>,
    /// Byte captures only; texture captures wait in the texture book.
    waiting: HashMap<u64, Function>,
    answers: Vec<(u64, Result<Vec<u8>, String>)>,
    /// Asked and not yet answered, both kinds, for the in-flight cap.
    open: usize,
    /// Texture book ids of the texture captures still open, so an answer for
    /// one can close it.
    open_textures: Vec<u64>,
    no_gpu: bool,
}

impl Captures {
    pub(crate) fn take_requests(&mut self) -> Vec<CaptureRequest> {
        std::mem::take(&mut self.requests)
    }

    pub(crate) fn set_can_draw(&mut self, can: bool) {
        self.no_gpu = !can;
    }

    /// The driver's answer for a byte capture.
    pub(crate) fn answer(&mut self, id: u64, result: Result<Vec<u8>, String>) {
        self.open = self.open.saturating_sub(1);
        self.answers.push((id, result));
    }

    /// The driver finished (or gave up on) a texture capture; its answer went
    /// through the texture book.
    pub(crate) fn texture_done(&mut self, tex_id: u64) {
        if let Some(i) = self.open_textures.iter().position(|&t| t == tex_id) {
            self.open_textures.swap_remove(i);
            self.open = self.open.saturating_sub(1);
        }
    }

    /// Scene load or Stop: the callbacks close over nodes that are gone. A
    /// picture already on the GPU still lands, and with nobody to tell is let
    /// go in [`drain`].
    pub(crate) fn cancel_all(&mut self) {
        self.waiting.clear();
        self.requests.clear();
        self.open = 0;
        self.open_textures.clear();
    }
}

/// Parse `([cam,] opts, cb)`.
fn parse_args(call: &str, args: mlua::MultiValue) -> mlua::Result<(CaptureCamera, Table, Function)> {
    let mut v: Vec<Value> = args.into_iter().collect();
    let cb = match v.pop() {
        Some(Value::Function(f)) => f,
        _ => {
            return Err(mlua::Error::RuntimeError(format!(
                "{call}: the last argument must be a function(result, err) — the picture arrives on a later frame"
            )));
        }
    };
    let opts = match v.pop() {
        Some(Value::Table(t)) => t,
        _ => {
            return Err(mlua::Error::RuntimeError(format!(
                "{call}: expected an options table {{ w = …, h = … }} before the callback"
            )));
        }
    };
    let camera = match v.pop() {
        None | Some(Value::Nil) => CaptureCamera::Active,
        Some(Value::String(s)) => CaptureCamera::Named(s.to_string_lossy().to_string()),
        Some(other) => match crate::env::node_id_of(&other) {
            Some(e) => CaptureCamera::Node(e),
            None => {
                return Err(mlua::Error::RuntimeError(format!(
                    "{call}: the camera must be a node, a camera's name, or left out for the active camera (got a {})",
                    other.type_name()
                )));
            }
        },
    };
    if !v.is_empty() {
        return Err(mlua::Error::RuntimeError(format!(
            "{call}: too many arguments — it takes ([camera,] options, callback)"
        )));
    }
    Ok((camera, opts, cb))
}

fn side(call: &str, opts: &Table, key: &str) -> mlua::Result<u32> {
    let v: Option<f64> = opts.get(key)?;
    let Some(v) = v else {
        return Err(mlua::Error::RuntimeError(format!("{call}: options.{key} is required (pixels)")));
    };
    if !(v.is_finite() && v >= 1.0 && v <= MAX_CAPTURE_SIDE as f64) {
        return Err(mlua::Error::RuntimeError(format!(
            "{call}: options.{key} must be between 1 and {MAX_CAPTURE_SIDE} pixels (got {v})"
        )));
    }
    Ok(v.round() as u32)
}

fn parse_format(call: &str, opts: &Table) -> mlua::Result<CaptureFormat> {
    let format: Option<String> = opts.get("format")?;
    let quality: Option<f64> = opts.get("quality")?;
    match format.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("png") => {
            if quality.is_some() {
                return Err(mlua::Error::RuntimeError(format!(
                    "{call}: options.quality applies to format = \"jpeg\" only — a PNG is lossless"
                )));
            }
            Ok(CaptureFormat::Png)
        }
        Some("jpeg") | Some("jpg") => {
            let q = quality.unwrap_or(90.0);
            if !(q.is_finite() && (1.0..=100.0).contains(&q)) {
                return Err(mlua::Error::RuntimeError(format!(
                    "{call}: options.quality must be between 1 and 100 (got {q})"
                )));
            }
            Ok(CaptureFormat::Jpeg(q.round() as u8))
        }
        Some(other) => Err(mlua::Error::RuntimeError(format!(
            "{call}: options.format must be \"png\" or \"jpeg\" (got \"{other}\")"
        ))),
    }
}

/// The keys a capture reads. Anything else is a typo, and a typo in `ui`
/// would put a HUD in every cover without a word.
const KEYS: &[&str] = &["w", "h", "format", "quality", "ui", "draws"];
const TEXTURE_KEYS: &[&str] = &["w", "h", "ui", "draws"];

fn check_keys(call: &str, opts: &Table, known: &[&str]) -> mlua::Result<()> {
    for pair in opts.pairs::<Value, Value>() {
        let (k, _) = pair?;
        let name = match &k {
            Value::String(s) => s.to_string_lossy().to_string(),
            other => format!("[{}]", other.type_name()),
        };
        if !known.contains(&name.as_str()) {
            return Err(mlua::Error::RuntimeError(format!(
                "{call}: unknown option \"{name}\" — it reads {}",
                known.join(", ")
            )));
        }
    }
    Ok(())
}

pub(crate) fn install(
    lua: &Lua,
    state: Rc<RefCell<Captures>>,
    textures: Rc<RefCell<TextureLoads>>,
) -> mlua::Result<()> {
    let camera: Table = lua.globals().get("camera")?;

    let st = state.clone();
    camera.set(
        "capture",
        lua.create_function(move |_, args: mlua::MultiValue| {
            const CALL: &str = "camera.capture";
            let (cam, opts, cb) = parse_args(CALL, args)?;
            check_keys(CALL, &opts, KEYS)?;
            let (w, h) = (side(CALL, &opts, "w")?, side(CALL, &opts, "h")?);
            let format = parse_format(CALL, &opts)?;
            let ui = opts.get::<Option<bool>>("ui")?.unwrap_or(false);
            let draws = opts.get::<Option<bool>>("draws")?.unwrap_or(false);
            let mut s = st.borrow_mut();
            s.next += 1;
            let id = s.next;
            s.waiting.insert(id, cb);
            if s.no_gpu {
                s.answers.push((id, Err(NO_RENDERER.into())));
                return Ok(());
            }
            if s.open >= MAX_CAPTURES_IN_FLIGHT {
                s.answers.push((
                    id,
                    Err(format!(
                        "{MAX_CAPTURES_IN_FLIGHT} captures are already waiting for the GPU — wait for one to answer"
                    )),
                ));
                return Ok(());
            }
            s.open += 1;
            s.requests.push(CaptureRequest { id, camera: cam, w, h, format, ui, draws, texture: None });
            Ok(())
        })?,
    )?;

    let st = state;
    camera.set(
        "captureTexture",
        lua.create_function(move |_, args: mlua::MultiValue| {
            const CALL: &str = "camera.captureTexture";
            let (cam, opts, cb) = parse_args(CALL, args)?;
            check_keys(CALL, &opts, TEXTURE_KEYS)?;
            let (w, h) = (side(CALL, &opts, "w")?, side(CALL, &opts, "h")?);
            let ui = opts.get::<Option<bool>>("ui")?.unwrap_or(false);
            let draws = opts.get::<Option<bool>>("draws")?.unwrap_or(false);
            // The texture book mints the name and keeps the callback, so the
            // picture is a texture like any other: released by
            // `assets.release`, let go on Stop.
            let mut book = textures.borrow_mut();
            let tex_id = book.start(cb);
            let mut s = st.borrow_mut();
            if s.no_gpu {
                book.answer(tex_id, Err(NO_RENDERER.into()));
                return Ok(());
            }
            if s.open >= MAX_CAPTURES_IN_FLIGHT {
                book.answer(
                    tex_id,
                    Err(format!(
                        "{MAX_CAPTURES_IN_FLIGHT} captures are already waiting for the GPU — wait for one to answer"
                    )),
                );
                return Ok(());
            }
            s.open += 1;
            s.open_textures.push(tex_id);
            s.requests.push(CaptureRequest {
                id: tex_id,
                camera: cam,
                w,
                h,
                format: CaptureFormat::Texture,
                ui,
                draws,
                texture: Some(format!("img:{tex_id}")),
            });
            Ok(())
        })?,
    )?;
    Ok(())
}

/// Call back every script whose picture has been answered: frame pass only.
pub(crate) fn drain(lua: &Lua, state: &Rc<RefCell<Captures>>, logs: &Rc<RefCell<Vec<ScriptLog>>>) {
    let ready: Vec<(Function, Result<Vec<u8>, String>)> = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        let answers = std::mem::take(&mut s.answers);
        answers.into_iter().filter_map(|(id, r)| s.waiting.remove(&id).map(|cb| (cb, r))).collect()
    };
    for (cb, result) in ready {
        let called = match result {
            Ok(bytes) => match lua.create_string(&bytes) {
                Ok(b) => cb.call::<()>((b, Value::Nil)),
                Err(e) => Err(e),
            },
            Err(why) => match lua.create_string(&why) {
                Ok(w) => cb.call::<()>((Value::Nil, w)),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = called {
            logs.borrow_mut().push(ScriptLog {
                level: LogLevel::Error,
                msg: format!("camera.capture callback: {e}"),
                source: None,
            });
        }
    }
}
