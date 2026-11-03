//! Files a game writes off the frame.
//!
//! ```lua
//! assets.writeBytes("user://cache/lod.bin", bytes, { async = true }, function(ok, err) end)
//! assets.writeJson("user://settings.json", t, { async = true })
//! ```
//!
//! A write to a player's file is made whole or not at all: a sibling is
//! written, flushed to the disk and renamed over the old one. The flush is the
//! slow part. On a spinning or busy disk it took hundreds of milliseconds for a
//! 30 KB file, all of it on the frame that asked: a game caching terrain as it
//! flew lost 918 ms in one frame. Asked for with `async = true` (or a
//! callback), the bytes are copied and written on a thread of their own, in
//! the order they were asked for, and the callback runs on a later frame.
//!
//! Reading a file whose write is still on its way waits for that write, so a
//! game never reads back the file it had before. A game that quits waits for
//! what it handed over. In a browser there are no threads: the write is made at
//! once and the callback still comes on a later frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Function, Lua};

use crate::{LogLevel, ScriptLog};

/// One write handed to the thread.
struct Job {
    id: u64,
    generation: u64,
    path: PathBuf,
    bytes: Vec<u8>,
    /// Replace the file whole (a player's file) or write it in place (the
    /// game's own folder, where a hidden sibling would show in the browser).
    atomic: bool,
    /// How the failure names it: `assets.writeBytes("user://…")`.
    what: String,
}

/// A write that has finished.
struct Done {
    id: u64,
    generation: u64,
    result: Result<(), String>,
}

fn perform(job: Job) -> Done {
    let wrote = if job.atomic { floptle_vfs::write_atomic(&job.path, &job.bytes) } else { floptle_vfs::write(&job.path, &job.bytes) };
    Done { id: job.id, generation: job.generation, result: wrote.map_err(|e| format!("{}: {e}", job.what)) }
}

/// The writes a host has handed over, and the callbacks waiting on them.
#[derive(Default)]
pub(crate) struct Writes {
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<Worker>,
    /// This session's callbacks, by write. Stop forgets them; the writes
    /// themselves still finish.
    callbacks: HashMap<u64, Function>,
    /// Every write handed over and not yet heard back from, in order.
    queued: Vec<(u64, PathBuf)>,
    /// Finished writes not yet delivered.
    ready: Vec<Done>,
    next_id: u64,
    generation: u64,
}

#[cfg(not(target_arch = "wasm32"))]
struct Worker {
    jobs: Option<std::sync::mpsc::Sender<Job>>,
    done: std::sync::mpsc::Receiver<Done>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Worker {
    fn start() -> std::io::Result<Self> {
        let (jobs, rx) = std::sync::mpsc::channel::<Job>();
        let (tx, done) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("file-writer".into()).spawn(move || {
            for job in rx {
                if tx.send(perform(job)).is_err() {
                    break;
                }
            }
        })?;
        Ok(Self { jobs: Some(jobs), done, thread: Some(thread) })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the queue ends the loop once it is empty: a game that quits
        // waits for the files it asked to be written.
        self.jobs = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Writes {
    /// Hand `bytes` to be written to `path`, calling `callback(ok, err)` on a
    /// later frame.
    pub(crate) fn submit(&mut self, path: PathBuf, bytes: Vec<u8>, atomic: bool, what: String, callback: Option<Function>) {
        self.next_id += 1;
        let id = self.next_id;
        if let Some(cb) = callback {
            self.callbacks.insert(id, cb);
        }
        let job = Job { id, generation: self.generation, path: path.clone(), bytes, atomic, what };
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.worker.is_none() {
                self.worker = Worker::start().ok();
            }
            let job = match self.worker.as_ref().and_then(|w| w.jobs.as_ref()) {
                Some(tx) => match tx.send(job) {
                    Ok(()) => {
                        self.queued.push((id, path));
                        return;
                    }
                    Err(back) => back.0,
                },
                None => job,
            };
            // No thread to be had: write it here.
            self.worker = None;
            self.ready.push(perform(job));
        }
        #[cfg(target_arch = "wasm32")]
        self.ready.push(perform(job));
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn heard(&mut self, done: Done) {
        if let Some(i) = self.queued.iter().position(|(id, _)| *id == done.id) {
            self.queued.remove(i);
        }
        self.ready.push(done);
    }

    /// Take in whatever has finished, without waiting.
    fn collect(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        while let Some(w) = self.worker.as_ref() {
            match w.done.try_recv() {
                Ok(d) => self.heard(d),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.worker = None;
                    break;
                }
            }
        }
    }

    /// Wait until no write to `path` is still on its way: a read of a file
    /// being written reads what was asked to be written, not what was there.
    pub(crate) fn settle(&mut self, path: &Path) {
        self.collect();
        // A page writes at once: nothing is ever on its way.
        #[cfg(target_arch = "wasm32")]
        let _ = path;
        #[cfg(not(target_arch = "wasm32"))]
        while self.queued.iter().any(|(_, p)| p == path) {
            let Some(w) = self.worker.as_ref() else { break };
            match w.done.recv() {
                Ok(d) => self.heard(d),
                Err(_) => {
                    self.worker = None;
                    self.queued.clear();
                    break;
                }
            }
        }
    }

    /// Stop: forget the callbacks. The writes still finish.
    pub(crate) fn cancel_all(&mut self) {
        self.generation += 1;
        self.callbacks.clear();
    }

    /// Writes handed over and not yet finished.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn in_flight(&self) -> usize {
        self.queued.len()
    }
}

/// Deliver finished writes: the frame pass, before `update`, so a callback's
/// own writes are seen the same frame. A failure with no callback to hear it
/// is said in the Console.
pub(crate) fn drain(_lua: &Lua, state: &Rc<RefCell<Writes>>, logs: &Rc<RefCell<Vec<ScriptLog>>>) {
    // Collect with the borrow held; call with it released — a callback that
    // writes again re-borrows the state.
    let ready: Vec<(Done, Option<Function>)> = {
        let mut s = state.borrow_mut();
        s.collect();
        let session = s.generation;
        let done = std::mem::take(&mut s.ready);
        done.into_iter().map(|d| {
            let cb = if d.generation == session { s.callbacks.remove(&d.id) } else { None };
            (d, cb)
        }).collect()
    };
    for (d, cb) in ready {
        match (cb, d.result) {
            (Some(cb), Ok(())) => {
                if let Err(e) = cb.call::<()>((true, mlua::Value::Nil)) {
                    logs.borrow_mut().push(ScriptLog { level: LogLevel::Error, msg: format!("write callback: {e}"), source: None });
                }
            }
            (Some(cb), Err(why)) => {
                if let Err(e) = cb.call::<()>((false, why)) {
                    logs.borrow_mut().push(ScriptLog { level: LogLevel::Error, msg: format!("write callback: {e}"), source: None });
                }
            }
            (None, Err(why)) => logs.borrow_mut().push(ScriptLog { level: LogLevel::Warn, msg: why, source: None }),
            (None, Ok(())) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A write handed over lands, in order, and a read waits for it.** Two
    /// writes to one file in a row leave the second; `settle` returns only
    /// once neither is on its way.
    #[test]
    fn writes_land_in_order_and_a_read_waits_for_them() {
        let dir = std::env::temp_dir().join(format!("floptle-writes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.bin");
        let mut w = Writes::default();
        w.submit(path.clone(), b"first".to_vec(), true, "test".into(), None);
        w.submit(path.clone(), b"second".to_vec(), true, "test".into(), None);
        w.settle(&path);
        assert_eq!(w.in_flight(), 0, "settle returned with a write still on its way");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
