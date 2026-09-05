//! Link the browser build against a C++ runtime that can throw.
//!
//! Luau is C++ and its parser reports a syntax error by throwing (caught inside
//! `luau_compile`; nothing escapes to Rust). Rust's `wasm32-unknown-unknown`
//! target ships no C++ runtime at all, so the one from the WASI SDK is linked
//! here: its `eh/` sysroot variant is built with WebAssembly exceptions
//! (wasi-sdk 33+; the stock `libc++abi.a` in older SDKs has no `__cxa_throw`).
//! The C++ itself is compiled for `wasm32-wasip1` — see `tools/web/env.sh` for
//! the compiler flags — and the objects are linked into this
//! `wasm32-unknown-unknown` module, which shares the wasm32 C ABI. The handful
//! of WASI imports that libc then needs are satisfied in `src/wasi.rs`.
//!
//! On any other target this script does nothing.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=WASI_SDK_PATH");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("wasm32") {
        return;
    }
    let sdk = std::env::var_os("WASI_SDK_PATH").map(PathBuf::from).unwrap_or_else(|| {
        panic!(
            "WASI_SDK_PATH is not set. The browser build links Luau against the WASI SDK's \
             C++ runtime — source tools/web/env.sh (it downloads the SDK on first use) and \
             build with tools/web/build.sh."
        )
    });
    let lib = sdk.join("share/wasi-sysroot/lib/wasm32-wasip1");
    let eh = lib.join("eh");
    assert!(
        eh.join("libc++abi.a").is_file(),
        "{} has no eh/libc++abi.a — the browser build needs wasi-sdk 33 or newer, whose C++ \
         runtime is built with WebAssembly exceptions",
        lib.display()
    );
    println!("cargo:rustc-link-search=native={}", eh.display());
    println!("cargo:rustc-link-search=native={}", lib.display());
    // Order matters to the linker: C++ first, then its ABI runtime and the
    // unwinder it calls into, then libc, which everything above reaches for.
    // `wasi-emulated-process-clocks` is what gives Luau's `os.clock` a
    // `clock()` — WASI proper has no process clock.
    for l in ["c++", "c++abi", "unwind", "wasi-emulated-process-clocks", "c"] {
        println!("cargo:rustc-link-arg=-l{l}");
    }
}
