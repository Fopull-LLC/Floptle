//! The executable's own icon on Windows (`branding/floptle.ico`), compiled in
//! as a resource so Explorer, the taskbar and the Start menu show the logo
//! for the file itself — a window icon set at runtime covers only the window.
//! Decided by the target, not the host: a Linux machine cross-building for
//! Windows still embeds it, given a `windres`; without one it warns and the
//! build goes on, because an icon is not worth a failed build.
//!
//! The Steam player also gets a search path: Steam's runtime library ships
//! beside the game's binary, and neither Linux nor macOS looks there unless
//! the binary says to. Windows always looks beside the executable.

fn main() {
    println!("cargo:rerun-if-changed=../../branding/floptle.ico");
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if std::env::var_os("CARGO_FEATURE_STEAM").is_some() {
        match os.as_str() {
            "linux" => println!("cargo:rustc-link-arg-bin=floptle-player-steam=-Wl,-rpath,$ORIGIN"),
            "macos" => println!("cargo:rustc-link-arg-bin=floptle-player-steam=-Wl,-rpath,@executable_path"),
            _ => {}
        }
    }
    if os != "windows" {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../branding/floptle.ico");
    if let Err(e) = res.compile() {
        println!("cargo:warning=the executable's icon was not embedded: {e}");
    }
}
