//! The Lua `app.*` table — the settings a game offers the person playing it, and
//! the one thing every main menu needs and could not do: **quit** (`floptle/0175`).
//!
//! ```lua
//! -- a Video tab, in full
//! function start(node)
//!   app.setVsync(save.get("vsync") or "On")
//!   app.setRetroHeight(save.get("pixelHeight") or app.retroHeight())
//! end
//!
//! function onQuit()
//!   save.flush()
//!   app.quit()
//! end
//! ```
//!
//! ## Why these live apart from the rest of the API
//!
//! Most of the script surface is about **simulating** a world. This is the other
//! kind: what the game presents to a player and lets them change about it.
//! Before this, a settings screen could reach the audio mixer (`audio.track`),
//! the accessibility settings (`access.*`) and the scene's own post-processing
//! (`node:getComponent("PostProcess")`) — three quarters of a real options menu —
//! and then had nothing at all for the two things a player looks for first, and
//! no way to close the game.
//!
//! ## The settings here are the PROJECT's, and a change is for this session only
//!
//! Vsync and the retro presentation live in `project.ron`, which ships to
//! everybody who plays. So a script changing one changes it **for the run**, and
//! Stop puts the project back exactly as it was — the same rule
//! `audio.track(…):setVolume` already follows, and for the same reason: this is a
//! player's preference, not an edit to the game.
//!
//! Which means **persisting it is the game's job**, through `save.*`. That is not
//! an omission; it is the only place a per-player setting belongs, and it is what
//! `access.*` decided for the same question.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::Lua;

/// How finished frames reach the display.
///
/// The API's own copy of the three modes, in the API's own crate. The file
/// format has its own (`floptle_scene::VsyncDoc`) and so does the renderer
/// (`floptle_render::Vsync`); each is one `match` from the next, and each is
/// owned by the layer that has to be able to change independently — the same
/// split every `…Doc` type in this engine already makes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Vsync {
    /// Every frame shown, in order, at the display's cadence.
    #[default]
    On,
    /// Render freely; the display takes the newest frame each refresh.
    Adaptive,
    /// Present the instant a frame is ready, tearing and all.
    Off,
}

impl Vsync {
    /// The name `project.ron` uses, which is the name a script says.
    pub fn name(self) -> &'static str {
        match self {
            Vsync::On => "On",
            Vsync::Adaptive => "Adaptive",
            Vsync::Off => "Off",
        }
    }

    /// Parse a name a script wrote. Case-insensitive, because "on" is what
    /// somebody types and refusing it would teach nothing.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "on" => Some(Vsync::On),
            "adaptive" => Some(Vsync::Adaptive),
            "off" => Some(Vsync::Off),
            _ => None,
        }
    }
}

/// What the game currently is — fed by the driver, read by `app.*`.
#[derive(Clone, Debug, Default)]
pub struct AppInfo {
    /// The game's title, for a menu to put at the top of itself.
    pub title: String,
    /// The engine version this build was made with.
    pub version: String,
    pub vsync: Vsync,
    /// Whether the retro presentation — compositing small and upscaling — is on.
    pub retro: bool,
    /// The internal height it composites at, in pixels. This engine's version of
    /// "resolution": in a pixel-art game it is the setting a player means.
    pub retro_height: u32,
    /// Upscale by a whole number and letterbox, rather than stretching.
    pub retro_integer_scale: bool,
    /// Whether the game's window currently covers the screen. Always `false`
    /// where there is no window (`floptle run`).
    pub fullscreen: bool,
}

/// What a script asked the driver to change or do this frame.
///
/// A request rather than a direct write, like `space.warp` and `physics.pause`
/// before it: every one of these touches something the driver owns (the swap
/// chain, a GPU target, the event loop), and none of them can be done from
/// inside a Lua call.
#[derive(Clone, Debug, Default)]
pub struct AppRequests {
    /// End the game. What that MEANS is the driver's call — see the module docs
    /// in the editor's `app_settings`.
    pub quit: bool,
    pub vsync: Option<Vsync>,
    pub retro: Option<bool>,
    pub retro_height: Option<u32>,
    pub retro_integer_scale: Option<bool>,
    /// Cover the screen (borderless, on the monitor the window is on), or
    /// go back to a window.
    pub fullscreen: Option<bool>,
}

impl AppRequests {
    /// Nothing asked for. Lets the driver skip the whole apply path on the
    /// ordinary frame, which is every frame but the one somebody clicks in.
    pub fn is_empty(&self) -> bool {
        !self.quit
            && self.vsync.is_none()
            && self.retro.is_none()
            && self.retro_height.is_none()
            && self.retro_integer_scale.is_none()
            && self.fullscreen.is_none()
    }
}

pub type SharedAppInfo = Rc<RefCell<AppInfo>>;
pub type SharedAppRequests = Rc<RefCell<AppRequests>>;

/// The smallest internal height worth compositing at, and the largest.
///
/// Refused rather than clamped, the way `access.setTextScale` refuses: a
/// settings slider hands over a number it already bounded, so one outside the
/// range means the caller computed it wrong — and a silently clamped value is a
/// slider that appears to stop working (`floptle/0082`).
pub const RETRO_HEIGHT_MIN: u32 = 32;
pub const RETRO_HEIGHT_MAX: u32 = 4320;

/// Install the `app` global.
pub fn install(lua: &Lua, info: &SharedAppInfo, req: &SharedAppRequests) -> mlua::Result<()> {
    let t = lua.create_table()?;

    // --- quit ---------------------------------------------------------------
    {
        let r = req.clone();
        t.set(
            "quit",
            lua.create_function(move |_, ()| {
                r.borrow_mut().quit = true;
                Ok(())
            })?,
        )?;
    }

    // --- what this game is --------------------------------------------------
    {
        let i = info.clone();
        t.set("title", lua.create_function(move |_, ()| Ok(i.borrow().title.clone()))?)?;
    }
    {
        let i = info.clone();
        t.set("version", lua.create_function(move |_, ()| Ok(i.borrow().version.clone()))?)?;
    }

    // --- vsync --------------------------------------------------------------
    {
        let i = info.clone();
        t.set("vsync", lua.create_function(move |_, ()| Ok(i.borrow().vsync.name()))?)?;
    }
    {
        let r = req.clone();
        let i = info.clone();
        t.set(
            "setVsync",
            lua.create_function(move |_, mode: String| {
                // Named rather than ignored. A misspelled mode that silently did
                // nothing is a settings menu whose control appears to work and
                // does not — the same failure an unknown layer name is refused
                // for.
                let Some(v) = Vsync::parse(&mode) else {
                    return Err(mlua::Error::RuntimeError(format!(
                        "app.setVsync({mode:?}) — the modes are \"On\", \"Adaptive\" and \"Off\""
                    )));
                };
                r.borrow_mut().vsync = Some(v);
                // Answered immediately by `app.vsync()`, so a menu that reads
                // back what it just set shows the new value on the same frame
                // rather than the old one. The driver still has to apply it.
                i.borrow_mut().vsync = v;
                Ok(())
            })?,
        )?;
    }

    // --- the retro presentation ---------------------------------------------
    {
        let i = info.clone();
        t.set("retro", lua.create_function(move |_, ()| Ok(i.borrow().retro))?)?;
    }
    {
        let r = req.clone();
        let i = info.clone();
        t.set(
            "setRetro",
            lua.create_function(move |_, on: bool| {
                r.borrow_mut().retro = Some(on);
                i.borrow_mut().retro = on;
                Ok(())
            })?,
        )?;
    }
    {
        let i = info.clone();
        t.set("retroHeight", lua.create_function(move |_, ()| Ok(i.borrow().retro_height))?)?;
    }
    {
        let r = req.clone();
        let i = info.clone();
        t.set(
            "setRetroHeight",
            lua.create_function(move |_, px: i64| {
                if !(RETRO_HEIGHT_MIN as i64..=RETRO_HEIGHT_MAX as i64).contains(&px) {
                    return Err(mlua::Error::RuntimeError(format!(
                        "app.setRetroHeight({px}) — between {RETRO_HEIGHT_MIN} and \
                         {RETRO_HEIGHT_MAX} pixels"
                    )));
                }
                r.borrow_mut().retro_height = Some(px as u32);
                i.borrow_mut().retro_height = px as u32;
                Ok(())
            })?,
        )?;
    }
    {
        let i = info.clone();
        t.set(
            "retroIntegerScale",
            lua.create_function(move |_, ()| Ok(i.borrow().retro_integer_scale))?,
        )?;
    }
    {
        let r = req.clone();
        let i = info.clone();
        t.set(
            "setRetroIntegerScale",
            lua.create_function(move |_, on: bool| {
                r.borrow_mut().retro_integer_scale = Some(on);
                i.borrow_mut().retro_integer_scale = on;
                Ok(())
            })?,
        )?;
    }
    // Fullscreen. The one Video setting every player looks for first, and the
    // one a settings menu could not offer until now — a reviewer said so, in
    // public, about a game whose menu had everything else.
    {
        let i = info.clone();
        t.set("fullscreen", lua.create_function(move |_, ()| Ok(i.borrow().fullscreen))?)?;
    }
    {
        let r = req.clone();
        let i = info.clone();
        t.set(
            "setFullscreen",
            lua.create_function(move |_, on: bool| {
                r.borrow_mut().fullscreen = Some(on);
                // Answered optimistically, the way the other setters are: the
                // driver applies it this frame, and a menu that reads the
                // setting back on the same frame it clicked should see its own
                // click.
                i.borrow_mut().fullscreen = on;
                Ok(())
            })?,
        )?;
    }

    lua.globals().set("app", t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names a script says are the names `project.ron` uses. A settings menu
    /// shows this string to a player and saves it; if the two vocabularies ever
    /// drifted, a saved setting would stop restoring.
    #[test]
    fn the_modes_are_the_names_the_project_file_uses() {
        for (name, mode) in
            [("On", Vsync::On), ("Adaptive", Vsync::Adaptive), ("Off", Vsync::Off)]
        {
            assert_eq!(Vsync::parse(name), Some(mode));
            assert_eq!(mode.name(), name, "the round trip has to come back the same");
        }
        // What somebody actually types.
        assert_eq!(Vsync::parse("on"), Some(Vsync::On));
        assert_eq!(Vsync::parse(" OFF "), Some(Vsync::Off));
        // And nothing else, so a misspelling can be named rather than ignored.
        assert_eq!(Vsync::parse("vsync"), None);
        assert_eq!(Vsync::parse("true"), None);
        assert_eq!(Vsync::parse(""), None);
    }

    /// The driver skips its whole apply path when nothing was asked for, which
    /// is every frame but the one somebody clicks in.
    /// **`app.setFullscreen` reaches the driver, and the getter answers the
    /// click on the same frame.** The plumbing half of the feature — the window
    /// half needs a window, which no test has. A menu that reads the setting
    /// back on the frame it clicked must see its own click, or its toggle
    /// flickers back for a frame.
    #[test]
    fn set_fullscreen_queues_a_request_and_answers_immediately() {
        let lua = Lua::new();
        let info: SharedAppInfo = Rc::new(RefCell::new(AppInfo::default()));
        let req: SharedAppRequests = Rc::new(RefCell::new(AppRequests::default()));
        install(&lua, &info, &req).unwrap();
        let before: bool = lua.load("return app.fullscreen()").eval().unwrap();
        assert!(!before, "a fresh info block is windowed");
        assert!(req.borrow().is_empty(), "reading is not asking");

        let after: bool = lua.load("app.setFullscreen(true) return app.fullscreen()").eval().unwrap();
        assert!(after, "the getter must reflect the click on the same frame");
        assert_eq!(req.borrow().fullscreen, Some(true), "the driver was not asked");
        assert!(!req.borrow().is_empty(), "a fullscreen request must not read as an empty frame");

        let back: bool = lua.load("app.setFullscreen(false) return app.fullscreen()").eval().unwrap();
        assert!(!back);
        assert_eq!(req.borrow().fullscreen, Some(false));
    }

    #[test]
    fn an_untouched_frame_asks_for_nothing() {
        let mut r = AppRequests::default();
        assert!(r.is_empty());
        r.retro_height = Some(240);
        assert!(!r.is_empty());
        assert!(!AppRequests { quit: true, ..Default::default() }.is_empty());
    }
}
