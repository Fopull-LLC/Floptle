//! The OS's own file pickers, always off the UI thread.
//!
//! **`rfd`'s synchronous API is never used, and cannot be.** The editor builds
//! rfd with `xdg-portal` + `tokio`, and in that combination every sync call —
//! `FileDialog::pick_folder()` and its siblings — is `pollster::block_on`
//! wrapped around the portal future, while the D-Bus transport underneath it is
//! tokio's. `pollster` is not a tokio runtime, so the first thing that future
//! touches panics with "there is no reactor running", the unwind escapes
//! winit's event loop, and the editor vanishes: no window, no dialog, no log
//! line. It is not a slow path or a degraded path. On Linux it is a crash on
//! click, every time, and it looks to the user like the button itself is broken.
//!
//! So every picker in the editor goes through here. A worker thread with its own
//! current-thread runtime drives the async dialog and sends the answer back down
//! a channel, which the caller drains on a later frame — cheap, because the
//! editor repaints continuously (`ControlFlow::Poll`), so there is always a next
//! frame whether or not the window has focus.
//!
//! Off-thread is also what the other platforms want. rfd's macOS backend hops to
//! the main thread by itself (`run_on_main`), whereas blocking the main thread
//! around a native modal is exactly how a Mac app wedges. Nobody has to hold
//! that distinction in their head: there is one way in, and it is this one.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

/// Ask for a single folder.
pub(crate) fn pick_folder(title: &str) -> Receiver<PathBuf> {
    let title = title.to_string();
    spawn(move || async move {
        rfd::AsyncFileDialog::new()
            .set_title(title)
            .pick_folder()
            .await
            .map(|h| h.path().to_path_buf())
    })
}

/// Ask for any number of files. An empty pick counts as a cancellation, because
/// it is one: there is nothing to import either way.
pub(crate) fn pick_files(title: &str) -> Receiver<Vec<PathBuf>> {
    let title = title.to_string();
    spawn(move || async move {
        rfd::AsyncFileDialog::new()
            .set_title(title)
            .pick_files()
            .await
            .map(|hs| hs.iter().map(|h| h.path().to_path_buf()).collect::<Vec<_>>())
            .filter(|paths| !paths.is_empty())
    })
}

/// Run a dialog on a thread that actually has a runtime, and hand back the
/// channel its answer arrives on.
///
/// Nothing is sent when the job yields `None` — the sender simply drops and the
/// channel disconnects, which is how [`poll`] tells "cancelled" from "still
/// open". A runtime that fails to build takes the same route, so a picker that
/// cannot open reads as a picker the user dismissed rather than as a hang.
fn spawn<T, Fut>(job: impl FnOnce() -> Fut + Send + 'static) -> Receiver<T>
where
    T: Send + 'static,
    Fut: std::future::Future<Output = Option<T>>,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        if let Some(v) = rt.block_on(job()) {
            let _ = tx.send(v);
        }
    });
    rx
}

/// What a dialog that is already open has to say this frame.
pub(crate) enum Answer<T> {
    /// Still open. Ask again next frame.
    Waiting,
    /// The user chose this.
    Chose(T),
    /// The user cancelled, or the picker could never open. Either way there is
    /// nothing to do and the receiver should be dropped.
    Closed,
}

/// Read a pending dialog without blocking. Drop the receiver after anything but
/// [`Answer::Waiting`].
pub(crate) fn poll<T>(rx: &Receiver<T>) -> Answer<T> {
    match rx.try_recv() {
        Ok(v) => Answer::Chose(v),
        Err(TryRecvError::Empty) => Answer::Waiting,
        Err(TryRecvError::Disconnected) => Answer::Closed,
    }
}

#[cfg(test)]
mod tests {
    /// **The guard for the bug this module exists to prevent.** rfd's sync API
    /// compiles perfectly, passes review, and dies the moment a Linux user
    /// clicks the button — so nothing but a check like this one catches it
    /// before a release does. It shipped once: both folder buttons in the
    /// package browser crashed the editor on click in v0.64.2.
    ///
    /// Scanning source text is a blunt instrument, and it is the right one
    /// here: the failure is a *call* that type-checks, on a platform CI cannot
    /// click a dialog on.
    #[test]
    fn no_source_file_calls_rfds_blocking_api() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                // This file names the forbidden types on purpose, to explain
                // them and to test for them.
                if path.file_name().is_some_and(|n| n == "native_dialog.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source");
                for (i, line) in text.lines().enumerate() {
                    if line.contains("rfd::FileDialog") || line.contains("rfd::MessageDialog") {
                        let rel = path.strip_prefix(&src).unwrap_or(&path);
                        offenders.push(format!("{}:{}", rel.display(), i + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "rfd's blocking API panics on Linux (no tokio reactor under the portal) and takes \
             the whole editor with it. Use crate::native_dialog instead. Found at: {}",
            offenders.join(", ")
        );
    }
}
