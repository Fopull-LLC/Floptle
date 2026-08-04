//! The Lua `access` table and the `caption` primitive — accessibility a game can
//! offer its players (`floptle/0079`).
//!
//! Before this the engine's entire accessibility surface was input rebinding, and
//! that exists by accident of the action-map work rather than by intent. A game
//! that wanted bigger text, a colourblind-safe picture, less movement or captions
//! had to build all four itself, so most would not.
//!
//! ```lua
//! -- an options menu, in full
//! function start(node)
//!   access.setTextScale(save.get("textScale") or 1.0)
//!   access.setColorFilter(save.get("colorFilter") or "none")
//!   access.setReducedMotion(save.get("reducedMotion") or false)
//!   access.setCaptions(save.get("captions") or false)
//! end
//! ```
//!
//! The engine honours what it owns: UI text sizes go through the layout, so text
//! scaling **reflows**; the colour filter is a stage in the post chain; UI
//! transitions snap when motion is reduced. What it cannot honour for a game — a
//! camera shake the game drives — reads `access.reducedMotion()` and skips it.
//!
//! Persisting these is the GAME's, via `save.*`, deliberately: they are the
//! player's settings and belong in the player's save, not in a project file that
//! ships to everyone.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::Lua;

use floptle_core::access::{Accessibility, ColorFilter};

/// The live accessibility settings, shared with the driver.
pub type SharedAccess = Rc<RefCell<Accessibility>>;

/// One queued caption: what to say and how long it stays up.
#[derive(Clone, Debug, PartialEq)]
pub struct Caption {
    pub text: String,
    pub seconds: f32,
}

/// Captions a script asked for this frame, drained by the driver.
pub type CaptionQueue = Rc<RefCell<Vec<Caption>>>;

/// Longest a caption may hold the screen. A line that never leaves is a line
/// covering the game.
const CAPTION_MAX_SECONDS: f64 = 30.0;

/// Install the `access` global and the `caption` function.
pub fn install(lua: &Lua, access: &SharedAccess, captions: &CaptionQueue) -> mlua::Result<()> {
    let t = lua.create_table()?;

    // --- text scale ---------------------------------------------------------
    {
        let a = access.clone();
        t.set("textScale", lua.create_function(move |_, ()| Ok(a.borrow().text_scale))?)?;
    }
    {
        let a = access.clone();
        t.set(
            "setTextScale",
            lua.create_function(move |_, v: f64| {
                // Refused rather than clamped: a settings slider hands over a
                // number it already bounded, so a value outside the range means
                // the caller computed it wrong — and a silently clamped 0.1 is a
                // slider that appears to stop working (`floptle/0082`).
                let v = crate::opts::require_range(
                    "access.setTextScale",
                    "scale",
                    v,
                    f64::from(Accessibility::TEXT_SCALE_MIN),
                    f64::from(Accessibility::TEXT_SCALE_MAX),
                )?;
                a.borrow_mut().text_scale = v as f32;
                Ok(())
            })?,
        )?;
    }

    // --- colour vision -----------------------------------------------------
    {
        let a = access.clone();
        t.set(
            "colorFilter",
            lua.create_function(move |_, ()| Ok(a.borrow().color_filter.name().to_string()))?,
        )?;
    }
    {
        let a = access.clone();
        t.set(
            "setColorFilter",
            lua.create_function(move |_, (name, strength): (String, Option<f64>)| {
                // Through the SAME parser the engine acts on, offering that
                // parser's own list — a misspelled filter that quietly meant
                // "off" is an accessibility setting that appears to do nothing.
                let f = crate::opts::parse_enum(
                    "access.setColorFilter",
                    "filter",
                    &name,
                    ColorFilter::ACCEPTS,
                    ColorFilter::parse,
                )?;
                let mut a = a.borrow_mut();
                a.color_filter = f;
                if let Some(s) = strength {
                    a.color_filter_strength = crate::opts::require_range(
                        "access.setColorFilter",
                        "strength",
                        s,
                        0.0,
                        1.0,
                    )? as f32;
                }
                Ok(())
            })?,
        )?;
    }
    {
        let a = access.clone();
        t.set(
            "colorFilterStrength",
            lua.create_function(move |_, ()| Ok(a.borrow().color_filter_strength))?,
        )?;
    }
    {
        // access.filters() — every name a settings dropdown can offer, in menu
        // order, so a game does not hard-code a list that can go stale.
        t.set(
            "filters",
            lua.create_function(|lua, ()| {
                let list = lua.create_table()?;
                for (i, f) in ColorFilter::ALL.iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("name", f.name())?;
                    row.set("label", f.label())?;
                    list.set(i + 1, row)?;
                }
                Ok(list)
            })?,
        )?;
    }

    // --- reduced motion ----------------------------------------------------
    {
        let a = access.clone();
        t.set("reducedMotion", lua.create_function(move |_, ()| Ok(a.borrow().reduced_motion))?)?;
    }
    {
        let a = access.clone();
        t.set(
            "setReducedMotion",
            lua.create_function(move |_, on: bool| {
                a.borrow_mut().reduced_motion = on;
                Ok(())
            })?,
        )?;
    }

    // --- captions ----------------------------------------------------------
    {
        let a = access.clone();
        t.set("captions", lua.create_function(move |_, ()| Ok(a.borrow().captions))?)?;
    }
    {
        let a = access.clone();
        t.set(
            "setCaptions",
            lua.create_function(move |_, on: bool| {
                a.borrow_mut().captions = on;
                Ok(())
            })?,
        )?;
    }

    lua.globals().set("access", t)?;

    // caption(text [, seconds]) — say a line, if the player asked for captions.
    //
    // Deliberately a no-op while captions are off, so a game writes `caption(...)`
    // beside the sound and never an `if` around it. The engine draws it, so every
    // game gets the same readable placement instead of hand-rolling one.
    {
        let a = access.clone();
        let q = captions.clone();
        lua.globals().set(
            "caption",
            lua.create_function(move |_, (text, secs): (String, Option<f64>)| {
                if !a.borrow().captions {
                    return Ok(false);
                }
                let secs = match secs {
                    Some(s) => crate::opts::require_range(
                        "caption",
                        "seconds",
                        s,
                        0.1,
                        CAPTION_MAX_SECONDS,
                    )?,
                    // Roughly reading speed: a beat, plus time per character.
                    None => (1.2 + text.chars().count() as f64 * 0.055).min(8.0),
                };
                q.borrow_mut().push(Caption { text, seconds: secs as f32 });
                Ok(true)
            })?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with() -> (Lua, SharedAccess, CaptionQueue) {
        let lua = Lua::new();
        let a: SharedAccess = Rc::new(RefCell::new(Accessibility::default()));
        let q: CaptionQueue = Rc::new(RefCell::new(Vec::new()));
        install(&lua, &a, &q).expect("install");
        (lua, a, q)
    }

    #[test]
    fn a_game_sets_and_reads_every_setting() {
        let (lua, a, _q) = lua_with();
        lua.load(
            "access.setTextScale(1.5)\n\
             access.setColorFilter('deuteranopia', 0.8)\n\
             access.setReducedMotion(true)\n\
             access.setCaptions(true)",
        )
        .exec()
        .expect("sets");
        let got = a.borrow();
        assert_eq!(got.text_scale, 1.5);
        assert_eq!(got.color_filter, ColorFilter::Deuteranopia);
        assert_eq!(got.color_filter_strength, 0.8);
        assert!(got.reduced_motion && got.captions);
        drop(got);
        // …and reads back what it set, which is what an options menu redraws from.
        let s: f32 = lua.load("return access.textScale()").eval().unwrap();
        assert_eq!(s, 1.5);
        let f: String = lua.load("return access.colorFilter()").eval().unwrap();
        assert_eq!(f, "deuteranopia");
        assert!(lua.load("return access.reducedMotion()").eval::<bool>().unwrap());
    }

    #[test]
    fn a_misspelled_filter_is_refused_not_read_as_off() {
        let (lua, a, _q) = lua_with();
        let err = lua
            .load("access.setColorFilter('deuteranope')")
            .exec()
            .expect_err("a near-miss must not mean `off`")
            .to_string();
        for want in ["filter", "deuteranope", "deuteranopia"] {
            assert!(err.contains(want), "missing {want:?}: {err}");
        }
        assert_eq!(a.borrow().color_filter, ColorFilter::None, "nothing was written");
    }

    #[test]
    fn an_out_of_range_text_scale_says_the_range() {
        let (lua, _a, _q) = lua_with();
        let err =
            lua.load("access.setTextScale(12)").exec().expect_err("out of range").to_string();
        assert!(err.contains("scale") && err.contains('3'), "{err}");
    }

    #[test]
    fn a_caption_is_queued_only_when_the_player_asked_for_captions() {
        let (lua, _a, q) = lua_with();
        // Off by default: the call is a no-op, so a game writes it unconditionally.
        let shown: bool = lua.load("return caption('a door unlocks')").eval().unwrap();
        assert!(!shown);
        assert!(q.borrow().is_empty());

        lua.load("access.setCaptions(true)").exec().unwrap();
        let shown: bool = lua.load("return caption('a door unlocks')").eval().unwrap();
        assert!(shown);
        let queued = q.borrow();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].text, "a door unlocks");
        assert!(
            queued[0].seconds > 1.0 && queued[0].seconds < 8.0,
            "a default duration should suit the length: {:?}",
            queued[0]
        );
    }

    #[test]
    fn the_filter_list_a_menu_shows_comes_from_the_engine() {
        let (lua, _a, _q) = lua_with();
        let n: usize = lua.load("return #access.filters()").eval().unwrap();
        assert_eq!(n, ColorFilter::ALL.len(), "a game must not have to hard-code the list");
        let first: String = lua.load("return access.filters()[1].name").eval().unwrap();
        assert_eq!(first, "none", "off comes first, as in every options menu");
    }
}
