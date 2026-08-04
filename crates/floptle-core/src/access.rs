//! Accessibility settings — text scale, colour-vision filter, reduced motion,
//! captions (`floptle/0079`).
//!
//! **Why these live in the engine.** Before this, the whole accessibility
//! surface was input rebinding, which exists by accident of the action-map work
//! rather than by intent. Searching the workspace for *colourblind*,
//! *text scale* and *reduced motion* returned zero hits each.
//!
//! Two reasons that is not acceptable. Console platform holders require a subset
//! of this, so a game built here could not pass certification without
//! hand-rolling all of it. And roughly 1 in 12 men has some colour vision
//! deficiency — an engine that calls this "the game's problem" pushes it onto
//! every game separately, and most will skip it.
//!
//! These are PLAYER settings, so a game's options menu drives them (`access.*`
//! in Lua) and the engine honours them in the parts it owns: the UI's text sizes
//! reflow, the post chain carries the filter, and UI transitions snap instead of
//! sliding. What the engine cannot honour for you — a game's own camera shake —
//! reads the same flag.

/// Which colour vision deficiency the picture is adjusted for.
///
/// The names are the ones a player recognises from an options menu, which is
/// deliberate: `Deuteranopia` is what somebody who has it will look for, and
/// "red-green mode" is what nobody calls it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorFilter {
    /// The picture is untouched.
    #[default]
    None,
    /// Red-blind (missing L cones). ~1% of men.
    Protanopia,
    /// Green-blind (missing M cones). The most common — ~6% of men.
    Deuteranopia,
    /// Blue-blind (missing S cones). Rare, ~0.01%.
    Tritanopia,
}

impl ColorFilter {
    /// Every spelling [`Self::parse`] accepts, for an error naming what it takes
    /// (`floptle/0082`).
    pub const ACCEPTS: &'static [&'static str] = &[
        "none",
        "off",
        "protanopia",
        "protan",
        "deuteranopia",
        "deutan",
        "tritanopia",
        "tritan",
    ];

    /// Every filter in menu order — what an options dropdown lists.
    pub const ALL: &'static [Self] =
        &[Self::None, Self::Protanopia, Self::Deuteranopia, Self::Tritanopia];

    /// Parse a filter name, case-insensitively. `None` for anything else — a
    /// misspelled filter that silently meant "off" is an accessibility setting
    /// that appears to do nothing.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "off" => Some(Self::None),
            "protanopia" | "protan" => Some(Self::Protanopia),
            "deuteranopia" | "deutan" => Some(Self::Deuteranopia),
            "tritanopia" | "tritan" => Some(Self::Tritanopia),
            _ => None,
        }
    }

    /// The name Lua and the Inspector use.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Protanopia => "protanopia",
            Self::Deuteranopia => "deuteranopia",
            Self::Tritanopia => "tritanopia",
        }
    }

    /// A human label for a settings dropdown.
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "off",
            Self::Protanopia => "protanopia (red-blind)",
            Self::Deuteranopia => "deuteranopia (green-blind)",
            Self::Tritanopia => "tritanopia (blue-blind)",
        }
    }

    /// The lane the post shader reads. 0 = off, so the filter pass is a no-op at
    /// its identity value like every other stage in the chain.
    pub fn lane(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Protanopia => 1,
            Self::Deuteranopia => 2,
            Self::Tritanopia => 3,
        }
    }
}

/// The player's accessibility settings.
///
/// Defaults are "everything off, text at 1×", so a project that never touches
/// this is exactly what it was before (`floptle/0079`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Accessibility {
    /// Multiplies every UI text size. Layout runs on the scaled size, so a
    /// `fit`-height box grows and its neighbours move down — text scaling that
    /// only made glyphs bigger inside the same box would just clip.
    pub text_scale: f32,
    /// Which colour vision deficiency the post chain adjusts for.
    pub color_filter: ColorFilter,
    /// How strongly (0 = off, 1 = full). A partial correction is a real setting:
    /// full daltonization shifts hues a lot, and some players want less.
    pub color_filter_strength: f32,
    /// Show the deficiency instead of correcting it — for the DEVELOPER, to see
    /// what a colourblind player sees. Not something to ship switched on.
    pub simulate_deficiency: bool,
    /// The player asked for less movement. The engine snaps its own UI
    /// transitions; a game reads this for its camera shake and screen effects.
    pub reduced_motion: bool,
    /// The player wants captions. `caption(...)` draws nothing while this is off,
    /// so a game does not need its own switch around every line.
    pub captions: bool,
}

impl Default for Accessibility {
    fn default() -> Self {
        Self {
            text_scale: 1.0,
            color_filter: ColorFilter::None,
            color_filter_strength: 1.0,
            simulate_deficiency: false,
            reduced_motion: false,
            captions: false,
        }
    }
}

impl Accessibility {
    /// Smallest text multiplier. Below this the UI is unreadable for everyone,
    /// which is not an accessibility setting.
    pub const TEXT_SCALE_MIN: f32 = 0.5;
    /// Largest text multiplier. 3× a 24-unit label is 72 units — most of a
    /// 720-unit canvas's height, which is as far as reflow can usefully go.
    pub const TEXT_SCALE_MAX: f32 = 3.0;

    /// Clamp every field into the range the engine will honour.
    pub fn clamped(mut self) -> Self {
        self.text_scale = if self.text_scale.is_finite() {
            self.text_scale.clamp(Self::TEXT_SCALE_MIN, Self::TEXT_SCALE_MAX)
        } else {
            1.0
        };
        self.color_filter_strength = if self.color_filter_strength.is_finite() {
            self.color_filter_strength.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self
    }

    /// Is anything switched on? (The editor shows a badge, and the post chain
    /// skips a pass it does not need.)
    pub fn any_on(&self) -> bool {
        self.text_scale != 1.0
            || self.color_filter != ColorFilter::None
            || self.reduced_motion
            || self.captions
    }

    /// Seconds a UI transition should take, given its authored duration.
    ///
    /// Reduced motion means **snap**, not "go faster": a 40 ms slide is still a
    /// slide, and vestibular triggers are about movement existing at all.
    pub fn transition_seconds(&self, authored: f32) -> f32 {
        if self.reduced_motion {
            0.0
        } else {
            authored
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_filter_name_parses() {
        for s in ColorFilter::ACCEPTS {
            assert!(ColorFilter::parse(s).is_some(), "offered but refused: {s:?}");
        }
        // …and each filter's own canonical name round-trips, so a setting saved
        // by name comes back as itself.
        for f in ColorFilter::ALL {
            assert_eq!(ColorFilter::parse(f.name()), Some(*f));
        }
        assert!(ColorFilter::parse("deuteranope").is_none(), "a near-miss is refused");
    }

    #[test]
    fn a_nonsense_text_scale_becomes_something_readable() {
        assert_eq!(Accessibility { text_scale: 0.0, ..Default::default() }.clamped().text_scale, 0.5);
        assert_eq!(Accessibility { text_scale: 99.0, ..Default::default() }.clamped().text_scale, 3.0);
        // NaN passes every comparison, so a clamp alone would keep it — and a
        // NaN text size measures as nothing and draws as nothing.
        assert_eq!(
            Accessibility { text_scale: f32::NAN, ..Default::default() }.clamped().text_scale,
            1.0
        );
    }

    #[test]
    fn reduced_motion_snaps_rather_than_hurries() {
        let a = Accessibility { reduced_motion: true, ..Default::default() };
        assert_eq!(a.transition_seconds(0.09), 0.0);
        assert_eq!(Accessibility::default().transition_seconds(0.09), 0.09);
    }

    #[test]
    fn the_default_is_the_engine_as_it_was() {
        let d = Accessibility::default();
        assert!(!d.any_on(), "nothing is on until a player turns it on");
        assert_eq!(d.text_scale, 1.0);
        assert_eq!(d.color_filter.lane(), 0, "lane 0 must be the no-op the shader skips");
    }
}
