//! The WASI a browser does not have.
//!
//! Luau's C++ is compiled for `wasm32-wasip1`, so its libc reaches for a
//! handful of `wasi_snapshot_preview1` imports: a clock, an environment, stdio.
//! The browser build links into `wasm32-unknown-unknown`, where nobody supplies
//! them — so they are defined here, in Rust, under the exact symbol names
//! wasi-libc declares its imports with. A defined symbol wins over an import at
//! link time, so the finished module imports nothing from WASI at all and the
//! page's loader needs no shim.
//!
//! Only what the linked module actually asks for is here (the list came from
//! the module's import section, not from the WASI spec), and each one answers
//! honestly: stdout/stderr go to the console, the clock is `performance.now`,
//! the environment is empty, files do not exist. Anything else libc might one
//! day reach for fails to link — loudly, by name — rather than being stubbed
//! into silence.

use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::wasm_bindgen;

unsafe extern "C" {
    /// The linker-synthesised entry that runs every C++ static constructor.
    fn __wasm_call_ctors();
}

/// Run the C++ static constructors, once.
///
/// **Referencing this symbol is load-bearing, not just calling it.** When
/// nothing in the module mentions `__wasm_call_ctors`, wasm-ld assumes a
/// *command*-style module and wraps every exported function in a call to it —
/// so the constructors would run again on every call the JS glue makes into
/// the module (`__externref_table_alloc`, `__wbindgen_malloc`, …): Luau's flag
/// registry re-registered each time, libc's clock re-sampled from inside the
/// global object's own initialisation. A single reference from a regular
/// object turns the wrapping off (lld/wasm/Writer.cpp, `createCommandExport-
/// Wrappers`), which is the reactor model wasi's `_initialize` uses. This is
/// that reference, and it must run before any C++ is touched.
pub fn run_static_constructors() {
    thread_local! { static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    if !DONE.replace(true) {
        // SAFETY: the linker guarantees the symbol; it is safe to call once
        // from the main thread before any C++ runs, which is what this is.
        unsafe { __wasm_call_ctors() };
    }
}

/// WASI errno values, the few that come up.
const ESUCCESS: i32 = 0;
const EBADF: i32 = 8;
const ESPIPE: i32 = 70;

#[repr(C)]
pub(crate) struct Ciovec {
    buf: *const u8,
    len: usize,
}

/// `fd_write`: stdout and stderr reach the console; anything else is not open.
///
/// # Safety
/// Called by libc with a valid iovec array of `n` entries and a valid
/// `written` pointer — the WASI contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __imported_wasi_snapshot_preview1_fd_write(
    fd: i32,
    iovs: *const Ciovec,
    n: i32,
    written: *mut usize,
) -> i32 {
    if fd != 1 && fd != 2 {
        return EBADF;
    }
    let mut text = Vec::new();
    for i in 0..n.max(0) as usize {
        // SAFETY: WASI hands a valid array of `n` iovecs.
        let iov = unsafe { &*iovs.add(i) };
        // SAFETY: each iovec names `len` readable bytes.
        text.extend_from_slice(unsafe { std::slice::from_raw_parts(iov.buf, iov.len) });
    }
    // SAFETY: `written` is a valid out-pointer per the WASI contract.
    unsafe { *written = text.len() };
    let s = String::from_utf8_lossy(&text);
    let s = JsValue::from_str(s.trim_end());
    if fd == 1 {
        web_sys::console::log_1(&s);
    } else {
        web_sys::console::error_1(&s);
    }
    ESUCCESS
}

#[unsafe(no_mangle)]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_close(_fd: i32) -> i32 {
    EBADF
}

#[unsafe(no_mangle)]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_fdstat_get(_fd: i32, _stat: *mut u8) -> i32 {
    EBADF
}

#[unsafe(no_mangle)]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_seek(
    _fd: i32,
    _offset: i64,
    _whence: i32,
    _new_offset: *mut i64,
) -> i32 {
    ESPIPE
}

#[wasm_bindgen]
extern "C" {
    /// `performance.now()`, bound directly. Not `web_sys::window()`: that goes
    /// through `js_sys::global()`, a lazily initialised cell, and this shim is
    /// reachable from a C++ static constructor (wasi-libc's process clock
    /// samples the time at start-up) — which can run while that very cell is
    /// being initialised. Measured, not imagined: it was a "reentrant init"
    /// panic on the first frame of the probe.
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    fn performance_now() -> f64;
}

/// `clock_time_get`, in nanoseconds. Wall time for `CLOCK_REALTIME` (id 0);
/// `performance.now` for the monotonic and process clocks — which is what
/// makes Luau's `os.clock` tick in a tab.
///
/// # Safety
/// `out` is a valid pointer to a `u64`, per the WASI contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __imported_wasi_snapshot_preview1_clock_time_get(
    id: i32,
    _precision: i64,
    out: *mut u64,
) -> i32 {
    let ms = if id == 0 { js_sys::Date::now() } else { performance_now() };
    // SAFETY: `out` is a valid out-pointer per the WASI contract.
    unsafe { *out = (ms * 1.0e6) as u64 };
    ESUCCESS
}

/// The environment is empty: no variables, no bytes.
///
/// # Safety
/// Both pointers are valid out-pointers, per the WASI contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __imported_wasi_snapshot_preview1_environ_sizes_get(
    count: *mut usize,
    bytes: *mut usize,
) -> i32 {
    // SAFETY: valid out-pointers per the WASI contract.
    unsafe {
        *count = 0;
        *bytes = 0;
    }
    ESUCCESS
}

#[unsafe(no_mangle)]
pub extern "C" fn __imported_wasi_snapshot_preview1_environ_get(
    _environ: *mut *mut u8,
    _buf: *mut u8,
) -> i32 {
    ESUCCESS
}

/// `proc_exit`: libc's `abort` and `exit` land here. A tab cannot exit, so the
/// honest answer is to throw — the page's error handler sees a message that
/// names the code, rather than a module that quietly stops calling back.
#[unsafe(no_mangle)]
pub extern "C" fn __imported_wasi_snapshot_preview1_proc_exit(code: i32) -> ! {
    wasm_bindgen::throw_str(&format!("the engine's C runtime called exit({code})"))
}
