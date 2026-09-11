//! `say!` and `say_err!` — `println!` and `eprintln!` that survive losing the
//! terminal.
//!
//! `eprintln!` **panics** when the write fails. The editor inherits its stderr
//! from whatever launched it — the Hub, a terminal that has since closed, a
//! pipe nobody reads — and the first burst of Console lines mirrored to a dead
//! descriptor took the whole process down with `failed printing to stderr`.
//! A log line is not worth the process; these write and discard the result.
//!
//! One crate with no dependencies, so the relay, the fleet agent and the Hub
//! can use it without pulling in the engine.

/// `println!`, minus the panic. The write's result is dropped on purpose.
#[macro_export]
macro_rules! say {
    () => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stdout().lock());
    }};
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stdout().lock(), $($arg)*);
    }};
}

/// `eprintln!`, minus the panic. The write's result is dropped on purpose.
#[macro_export]
macro_rules! say_err {
    () => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr().lock());
    }};
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr().lock(), $($arg)*);
    }};
}

#[cfg(test)]
mod tests {
    /// Point this process's stderr at a pipe nobody reads. A write then fails
    /// with `EPIPE` — the same failing write a dead pty gives as `EIO` — and
    /// that is what `eprintln!` panics on. (A merely CLOSED descriptor is
    /// not enough: Rust's stdio swallows `EBADF` on purpose.)
    #[cfg(unix)]
    fn break_stderr() {
        unsafe extern "C" {
            fn pipe(fds: *mut i32) -> i32;
            fn close(fd: i32) -> i32;
            fn dup2(from: i32, to: i32) -> i32;
        }
        let mut fds = [0i32; 2];
        // SAFETY: plain POSIX calls on descriptors this child process owns;
        // every write to fd 2 after this fails, which is the condition under
        // test. Rust ignores SIGPIPE at startup, so the failure is an error
        // rather than a signal.
        unsafe {
            assert_eq!(pipe(fds.as_mut_ptr()), 0);
            close(fds[0]);
            assert_eq!(dup2(fds[1], 2), 2);
        }
    }

    /// The whole reason the crate exists, proved on a descriptor that is gone:
    /// stderr is closed underneath us and ten thousand lines go nowhere, with
    /// no panic. `eprintln!` in the same position aborts the test with
    /// "failed printing to stderr".
    ///
    /// Closing fd 2 for the rest of the test process is a side effect other
    /// tests would feel, so this runs in a child process of its own.
    #[cfg(unix)]
    #[test]
    fn writing_to_a_broken_stderr_does_not_panic() {
        if std::env::var("FLOPTLE_SAY_CHILD").is_ok() {
            break_stderr();
            for i in 0..10_000 {
                say_err!("line {i}");
                say!("line {i}");
            }
            std::process::exit(42);
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("tests::writing_to_a_broken_stderr_does_not_panic")
            .arg("--exact")
            .arg("--nocapture")
            .env("FLOPTLE_SAY_CHILD", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(42), "the child did not reach the end: {status:?}");
    }

    /// The control: the macro this replaces DOES die on the same descriptor,
    /// or the test above proves nothing about what it fixed.
    #[cfg(unix)]
    #[test]
    fn the_control_eprintln_panics_on_a_broken_stderr() {
        if std::env::var("FLOPTLE_SAY_CONTROL").is_ok() {
            break_stderr();
            for i in 0..10_000 {
                eprintln!("line {i}");
            }
            std::process::exit(42);
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("tests::the_control_eprintln_panics_on_a_broken_stderr")
            .arg("--exact")
            .arg("--nocapture")
            .env("FLOPTLE_SAY_CONTROL", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_ne!(status.code(), Some(42), "eprintln! survived a broken stderr — the control is wrong");
    }
}

/// The sweep, kept swept: no engine crate reaches for `println!`/`eprintln!`
/// outside its tests. A `clippy.toml` `disallowed-macros` would say the same
/// thing, but it would say it about every example and test as well — and a
/// probe that prints its numbers is doing its job.
#[cfg(test)]
mod sweep_guard {
    /// Where a file's test module starts: the first `#[cfg(test)]` (or
    /// `#[cfg(all(test, …))]`) directly followed by a `mod name {`. Everything
    /// before it is shipped code. A `#[cfg(test)] mod x;` declaration is not
    /// a boundary — the body is elsewhere.
    fn shipped_part(src: &str) -> &str {
        let mut at = 0;
        while let Some(i) = src[at..].find("#[cfg(") {
            let start = at + i;
            let rest = &src[start..];
            let is_test = rest.starts_with("#[cfg(test)]") || rest.starts_with("#[cfg(all(test");
            if is_test && let Some(nl) = rest.find('\n') {
                let next = rest[nl + 1..].trim_start();
                let next = next.strip_prefix("pub ").unwrap_or(next);
                let next = next.strip_prefix("pub(crate) ").unwrap_or(next);
                if let Some(after_mod) = next.strip_prefix("mod ")
                    && let Some(line_end) = after_mod.find('\n')
                    && after_mod[..line_end].trim_end().ends_with('{')
                {
                    return &src[..start];
                }
            }
            at = start + 6;
        }
        src
    }

    #[test]
    fn no_shipped_engine_code_uses_println_or_eprintln() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        let mut stack = vec![crates.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    // Crate roots, and `src/` trees under them — examples and
                    // integration tests print on purpose.
                    let under_src = p.components().any(|c| c.as_os_str() == "src");
                    if under_src || p.parent() == Some(crates.as_path()) {
                        stack.push(p);
                    }
                    continue;
                }
                if p.extension().and_then(|x| x.to_str()) != Some("rs")
                    || p.file_name().and_then(|n| n.to_str()) == Some("main.rs")
                    || p.starts_with(crates.join("floptle-say"))
                    || !p.components().any(|c| c.as_os_str() == "src")
                {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&p) else { continue };
                scanned += 1;
                for (n, line) in shipped_part(&src).lines().enumerate() {
                    let t = line.trim_start();
                    if t.starts_with("//") {
                        continue;
                    }
                    for needle in ["eprintln!(", "println!("] {
                        if let Some(i) = line.find(needle) {
                            let before = line[..i].chars().next_back();
                            if before.is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == ':')) {
                                offenders.push(format!("{}:{}: {}", p.display(), n + 1, t));
                            }
                        }
                    }
                }
            }
        }
        assert!(scanned > 100, "the walk found {scanned} files — it has stopped seeing the crates");
        assert!(
            offenders.is_empty(),
            "shipped code prints with a macro that panics when the terminal is gone — use \
             floptle_say::say!/say_err! instead:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn the_boundary_is_a_test_module_with_a_body_not_a_declaration() {
        let decl = "fn a() {}\n#[cfg(test)]\nmod probes;\nfn b() {}\n";
        assert_eq!(shipped_part(decl), decl);
        let body = "fn a() {}\n#[cfg(test)]\nmod tests {\n  fn t() {}\n}\n";
        assert_eq!(shipped_part(body), "fn a() {}\n");
        let all = "fn a() {}\n#[cfg(all(test, unix))]\npub(crate) mod t {\n}\n";
        assert_eq!(shipped_part(all), "fn a() {}\n");
        let feature = "fn a() {}\n#[cfg(feature = \"x\")]\nmod x {\n}\n";
        assert_eq!(shipped_part(feature), feature);
    }
}
