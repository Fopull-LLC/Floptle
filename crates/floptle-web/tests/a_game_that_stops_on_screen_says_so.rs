//! A web game that stops after it is on screen says so, and the next visit
//! can show why.
//!
//! The page used to report an engine trap only while its loading panel was
//! up. A trap during a level load, after the menu had been shown, stopped the
//! game with nothing on screen to say so. The canvas kept its last frame,
//! which is the game's own loading screen, so on a phone the game looked
//! stuck on loading forever. The run's record was then closed as a clean exit
//! when the player gave up, and the next visit said nothing either.
//!
//! An engine error that does not stop the game (a shader the browser refused,
//! which can leave the screen black) was not in the record at all: the engine
//! wrote it to a stderr a browser does not have.
//!
//! These read the page's source. `tools/web/shot.py` is where the behaviour
//! itself was checked, by making a running game's page trap and reload.

const PAGE: &str = include_str!("../web/index.html");

/// The page's `addEventListener('error', …)` handler, comments removed.
fn error_handler() -> String {
    let start = PAGE.find("window.addEventListener('error'").expect("index.html listens for errors");
    let body = &PAGE[start..];
    let end = body.find("\n  });").expect("the error handler ends");
    body[..end].lines().map(|l| l.split("// ").next().unwrap_or("")).collect::<Vec<_>>().join("\n")
}

#[test]
fn an_engine_trap_is_shown_after_the_game_is_on_screen_too() {
    let handler = error_handler();
    assert!(handler.contains("fail("), "the error handler shows the player that the game stopped");
    assert!(
        !handler.contains("!shown"),
        "the error handler only acts while the loading panel is up, so a trap during a level load \
         leaves the game frozen on its own loading screen with nothing said"
    );
}

#[test]
fn an_engine_error_is_kept_for_the_next_visit() {
    let log = PAGE.find("window.floptleLog = ").expect("index.html defines floptleLog");
    let log = &PAGE[log..log + PAGE[log..].find("\n  };").expect("floptleLog ends")];
    assert!(
        log.contains("'floptle: error:'") && log.contains("record.problems"),
        "floptleLog counts the engine's errors into the run's record"
    );
    assert!(
        PAGE.contains("lastRun.problems > 0"),
        "the next visit offers the record of a run whose engine reported errors, not only one that crashed"
    );
}
