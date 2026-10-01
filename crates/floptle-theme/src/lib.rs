//! Themes for the Floptle editor and Hub.
//!
//! One crate both programs depend on, so the two cannot drift apart the way
//! they drifted from fopull.com (contract `floptle-brand` §7.8). It holds:
//!
//! - the theme format ([`model`]), and where themes come from ([`source`]):
//!   built in, your themes folder, or a package; a theme is a folder with a
//!   `theme.ron`, or that folder zipped as a `.floptletheme` to share;
//! - the brand's fonts ([`fonts`]) and the egui style a theme makes
//!   ([`style`]);
//! - painting a region ([`paint`]) and the GPU backdrops behind them
//!   ([`backdrop`]);
//! - the brand's components ([`look`]) and the theme settings UI ([`ui`]).
//!
//! A host does four things: load [`Prefs`] and a [`Library`], [`apply`] the
//! chosen theme and set its fonts, call [`paint_region`] for each panel it
//! draws, and run a [`BackdropRenderer`] before drawing egui.

pub mod backdrop;
pub mod color;
pub mod fonts;
pub mod host;
pub mod look;
pub mod model;
pub mod paint;
pub mod source;
pub mod style;
pub mod ui;

use std::sync::{Arc, OnceLock};

pub use backdrop::BackdropRenderer;
pub use color::Rgba;
pub use model::{Theme, ThemeError, ThemeFile, Tokens};
pub use paint::{apply, paint_region, region_fill, region_has_layers, region_shapes, repaint_after, theme};
pub use source::{Effects, Library, Prefs, Source};
pub use style::signal;

/// Floptle Dark, resolved once.
pub fn default_theme() -> Arc<Theme> {
    static T: OnceLock<Arc<Theme>> = OnceLock::new();
    T.get_or_init(|| {
        Arc::new(
            source::load(&Source::Builtin(source::DEFAULT_ID), model::Origin::Builtin, true)
                .expect("Floptle Dark is built in and a test proves it loads"),
        )
    })
    .clone()
}

#[cfg(test)]
mod tests;
