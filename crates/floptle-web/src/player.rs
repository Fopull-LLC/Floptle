//! `play(bundle)`: the game, from a page.
//!
//! The page fetched the bundle the export packed (`game.flpk`) and, on the
//! player's click, hands the bytes here. They become the filesystem, the
//! manifest at their root names the game, and the desktop's own player starts
//! on the page's canvas. Nothing about the engine knows it is in a browser
//! past this point — that is what `floptle_vfs`, `floptle_core::time` and the
//! worker seam bought.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// `window.floptleFatal`, defined by the player page: the game stopped,
    /// and this is why. Optional — the probe page does not define it.
    #[wasm_bindgen(js_namespace = window, js_name = floptleFatal, catch)]
    fn page_fatal(text: &str) -> Result<(), JsValue>;
}

/// Tell the page the game stopped. Silent where the page has no such hook.
pub(crate) fn fatal(text: &str) {
    let _ = page_fatal(text);
}

/// The engine's `log::warn!`/`info!` lines, to the browser console — there is
/// no stderr here for `env_logger` to write to.
struct ConsoleLogger;

impl log::Log for ConsoleLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = wasm_bindgen::JsValue::from_str(&format!("{} {}", r.level(), r.args()));
        match r.level() {
            log::Level::Error => web_sys::console::error_1(&line),
            log::Level::Warn => web_sys::console::warn_1(&line),
            _ => web_sys::console::log_1(&line),
        }
    }
    fn flush(&self) {}
}

static LOGGER: ConsoleLogger = ConsoleLogger;

/// Start the game in `bundle` on the page's `<canvas id="game">`.
///
/// Returns once the event loop is handed to the browser; the game runs from
/// its animation frames. A `JsValue` error is one sentence for the page to
/// show: not a bundle, no manifest, no canvas.
#[wasm_bindgen]
pub fn play(bundle: Vec<u8>) -> Result<(), JsValue> {
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
    let n = floptle_vfs::mount(bundle).map_err(|e| JsValue::from_str(&e))?;
    crate::probe::log(&format!("floptle-web {} — {n} files in the bundle", env!("CARGO_PKG_VERSION")));
    let canvas = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("game"))
        .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        .ok_or_else(|| JsValue::from_str("the page has no <canvas id=\"game\">"))?;
    floptle_editor::player::web::start(canvas).map_err(|e| JsValue::from_str(&e))
}
