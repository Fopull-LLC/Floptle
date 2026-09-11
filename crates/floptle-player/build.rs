//! The executable's own icon on Windows (`branding/floptle.ico`), compiled in
//! as a resource so Explorer, the taskbar and the Start menu show the logo
//! for the file itself — a window icon set at runtime covers only the window.
//! Decided by the TARGET, not the host: a Linux machine cross-building for
//! Windows still embeds it, given a `windres`; without one it warns and the
//! build goes on, because an icon is not worth a failed build.

fn main() {
    println!("cargo:rerun-if-changed=../../branding/floptle.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../branding/floptle.ico");
    if let Err(e) = res.compile() {
        println!("cargo:warning=the executable's icon was not embedded: {e}");
    }
}
