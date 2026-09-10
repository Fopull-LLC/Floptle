//! The loop: ask what should be running, make it so, say what is.
//!
//! ## Four steps, and the fourth is not optional
//!
//! 1. `GET /desired` — what this region is supposed to be running.
//! 2. Ensure each deployment's engine version and bundle are on the box.
//! 3. Reconcile the unit set: start what should run, stop what should not.
//! 4. `POST /status` — what actually happened.
//!
//! Step 4 reads as reporting and is really part of the machinery: W releases a
//! deployment's UDP port five minutes after **this agent** says the process is
//! gone, because handing a live port on while the old server's players are
//! still sending packets delivers their traffic into a different game. An agent
//! that reconciled perfectly and never reported would leak a port per stopped
//! deployment until somebody noticed by hand.
//!
//! ## What it does when it cannot reach the control plane
//!
//! Nothing. A failed poll leaves every running server running and tries again
//! next cycle. The alternative — treating "I could not ask" as "nothing should
//! be running" — would take a whole region down on a bad minute at the website,
//! which is the same mistake `floptle/0189` records on the entitlements
//! endpoint and the same answer: absent is not revoked.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::args::Args;
use crate::bundle;
use crate::unit;
use crate::wire::{BoxStats, Deployment, DeploymentStatus, Desired, Report, ServerStatus, State};

/// What the agent remembers between cycles.
#[derive(Default)]
pub struct Agent {
    /// Restarts observed per deployment, so the report can carry the number a
    /// developer actually wants ("it has crashed four times").
    restarts: BTreeMap<String, u32>,
    /// Deployments that were running last cycle, so one that has left
    /// `/desired` can be reported terminal ONCE before it is forgotten.
    seen: BTreeMap<String, State>,
}

/// Anything that runs a command — real systemd, or a recorder in a test.
///
/// The agent is a reconciler, and reconcilers are exactly the programs that are
/// impossible to test if they reach for a global. Everything that changes the
/// box goes through this.
pub trait Host {
    fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String>;
}

/// The real box.
pub struct RealHost;

impl Host for RealHost {
    fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
        let out = std::process::Command::new(prog)
            .args(args)
            .output()
            .map_err(|e| format!("{prog}: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "{prog} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

impl Agent {
    /// One cycle. Returns the report that was sent, so `--once` and the tests
    /// can look at it.
    pub fn cycle(
        &mut self,
        args: &Args,
        host: &mut dyn Host,
        desired: &Desired,
    ) -> Result<Report, String> {
        let mut statuses = Vec::new();
        let mut wanted: Vec<String> = Vec::new();

        for d in &desired.deployments {
            wanted.push(unit::unit_name(&d.deployment_id));
            match self.ensure(args, host, d) {
                Ok(st) => statuses.push(st),
                Err(e) => {
                    // A deployment that could not be prepared is `failed` with
                    // the reason in its own log lines, rather than absent from
                    // the report — absent tells the portal nothing, and the
                    // developer is looking at a page that says "starting".
                    bundle::log_line(&format!("deployment {}: {e}", d.deployment_id));
                    self.seen.insert(d.deployment_id.clone(), State::Failed);
                    statuses.push(DeploymentStatus {
                        deployment_id: d.deployment_id.clone(),
                        state: State::Failed.as_str(),
                        peers: 0,
                        uptime_s: 0,
                        restarts: *self.restarts.get(&d.deployment_id).unwrap_or(&0),
                        tick_p95_ms: None,
                        last_lines: vec![e],
                        lobby_code: None,
                        port: None,
                        relay: None,
                    });
                }
            }
        }

        // **Everything this agent owns that is no longer wanted.** Enumerating
        // its own units is what makes removal possible at all; without it a
        // stopped deployment runs forever, holding a port W thinks is free.
        for name in unit::owned_units(&args.units) {
            if wanted.contains(&name) {
                continue;
            }
            let id = name
                .strip_prefix("floptle-d-")
                .and_then(|s| s.strip_suffix(".service"))
                .unwrap_or_default()
                .to_string();
            bundle::log_line(&format!("stopping {name} — no longer in /desired"));
            if !args.dry_run {
                let _ = host.run("systemctl", &["disable", "--now", &name]);
                let _ = std::fs::remove_file(args.units.join(&name));
            }
            // Reported terminal exactly once, then forgotten. This is the
            // report that releases the port.
            if self.seen.remove(&id).is_some_and(|s| !s.is_terminal()) || !id.is_empty() {
                statuses.push(DeploymentStatus {
                    deployment_id: id,
                    state: State::Stopped.as_str(),
                    peers: 0,
                    uptime_s: 0,
                    restarts: 0,
                    tick_p95_ms: None,
                    last_lines: vec!["stopped by the control plane".into()],
                    lobby_code: None,
                    port: None,
                    relay: None,
                });
            }
            if !args.dry_run {
                let _ = host.run("systemctl", &["daemon-reload"]);
            }
        }

        Ok(Report { box_: box_stats(host), deployments: statuses })
    }

    /// Get one deployment to the state `/desired` asks for, and say what it is
    /// doing.
    fn ensure(
        &mut self,
        args: &Args,
        host: &mut dyn Host,
        d: &Deployment,
    ) -> Result<DeploymentStatus, String> {
        let bundle_dir = bundle::dir_for(&args.root, &d.sha256);
        if !bundle::present(&args.root, &d.sha256) {
            bundle::log_line(&format!("fetching bundle for {}", d.redacted()));
            if args.dry_run {
                return Ok(self.status_of(args, host, d, State::Starting));
            }
            crate::fetch::fetch_bundle(&d.build_url, &d.sha256, &bundle_dir)?;
        } else if !args.dry_run && bundle::ensure_readable(&bundle_dir)? {
            // Unpacked by an agent that trusted the archive's mode bits
            // (`floptle/0200`, defect two). Once, then the marker says so.
            bundle::log_line(&format!("{}: made the bundle readable by the server", d.redacted()));
        }

        // **The bundle's own manifest wins over the row for scene and project.**
        // The control plane read both out of this same file at upload time, so
        // a disagreement means the row is stale — and starting a server against
        // a scene the build does not contain is a crash-loop whose page says
        // "crashed" with nothing a developer can act on.
        let manifest = bundle::read_manifest(&bundle_dir)?;
        let mut d = d.clone();
        if let Some(p) = manifest.project.clone() {
            if p != d.project {
                bundle::log_line(&format!(
                    "deployment {}: the row says project {:?}, the bundle says {:?} — using the bundle's",
                    d.deployment_id, d.project, p
                ));
            }
            d.project = p;
        }
        let scene = manifest.scene.clone().or_else(|| d.args.scene.clone());
        let project_dir = bundle_dir.join(&d.project);
        if !project_dir.is_dir() {
            return Err(format!(
                "the bundle names project {:?} and has no such directory in it",
                d.project
            ));
        }
        if let Some(s) = &scene
            && !project_dir.join(s).is_file()
        {
            return Err(format!(
                "the bundle's scene {s:?} is not in it — a server started on it would crash, \
                 restart and crash, and every one of those is a page saying \"crashed\""
            ));
        }

        let server_bin = crate::engine::ensure_engine(args, &d.engine_version)?;

        // The parent only. The per-deployment directory under it is systemd's
        // to create, owned by the server's own user — a directory the agent
        // made here would be root's, which is the bug this replaces.
        std::fs::create_dir_all(&args.run).map_err(|e| format!("create run dir: {e}"))?;
        let runtime_dir = args.runtime_directory(&d.deployment_id);
        if runtime_dir.is_none() {
            bundle::log_line(&format!(
                "warning: --run {} is not under /run, so systemd cannot hand the server a \
                 directory there and {} will not be written",
                args.run.display(),
                d.deployment_id
            ));
        }
        let plan = unit::UnitPlan {
            dep: &d,
            server_bin,
            bundle_dir,
            status_file: args.status_file(&d.deployment_id),
            scene,
            relay: args.relay.clone(),
            runtime_dir,
        };
        let text = unit::render(&plan);
        let name = unit::unit_name(&d.deployment_id);
        let path = args.units.join(&name);

        // Rewrite only on a real change: `daemon-reload` and a restart on every
        // ten-second poll would bounce every server on the box, forever.
        let changed = std::fs::read_to_string(&path).map(|old| old != text).unwrap_or(true);
        if args.dry_run {
            bundle::log_line(&format!(
                "would {} {name}",
                if changed { "write and (re)start" } else { "leave" }
            ));
            return Ok(self.status_of(args, host, &d, State::Starting));
        }
        if changed {
            std::fs::create_dir_all(&args.units).map_err(|e| format!("create unit dir: {e}"))?;
            std::fs::write(&path, &text).map_err(|e| format!("write {}: {e}", path.display()))?;
            host.run("systemctl", &["daemon-reload"])?;
            host.run("systemctl", &["enable", &name])?;
            // **`restart`, not `enable --now`.** `--now` means "start it if it
            // is not running", and on a unit that is already active it does
            // NOTHING — so the agent would write a corrected unit, log that it
            // had started it, and leave the old process running the old command
            // line forever. That is not a hypothetical: it is what happened on
            // `us-east-1` when `floptle/0200`'s fix first reached the box, and
            // the fix read as a failure because the file on disk was right and
            // the running process was a day old. Reaching this branch at all
            // means the text CHANGED, which means the running process is
            // serving something other than what the control plane asked for.
            host.run("systemctl", &["restart", &name])?;
            bundle::log_line(&format!("{name}: written and (re)started"));
        } else {
            // Present and unchanged — but it may have been stopped by hand or
            // never started, so ask rather than assume.
            if unit_state(host, &name) != State::Running {
                let _ = host.run("systemctl", &["start", &name]);
            }
        }

        let state = unit_state(host, &name);
        Ok(self.status_of(args, host, &d, state))
    }

    /// Assemble one deployment's line of the report.
    fn status_of(
        &mut self,
        args: &Args,
        host: &mut dyn Host,
        d: &Deployment,
        state: State,
    ) -> DeploymentStatus {
        let id = d.deployment_id.clone();
        // A transition INTO a run from a non-running state is a restart, which
        // is the number a developer actually wants: "it has crashed four times"
        // rather than "it is up".
        if state == State::Running && self.seen.get(&id).is_some_and(|p| *p == State::Failed) {
            *self.restarts.entry(id.clone()).or_default() += 1;
        }
        self.seen.insert(id.clone(), state);

        let s = read_server_status(&args.status_file(&id));
        DeploymentStatus {
            deployment_id: id.clone(),
            state: state.as_str(),
            peers: s.peers,
            uptime_s: s.uptime_s,
            restarts: *self.restarts.get(&id).unwrap_or(&0),
            tick_p95_ms: s.tick_p95_ms,
            last_lines: journal_tail(host, &unit::unit_name(&id)),
            lobby_code: s.lobby_code,
            port: s.port,
            relay: s.relay,
        }
    }
}

/// What systemd says one unit is doing, in the control plane's words.
///
/// `activating` is `starting` and not `running`: a deployment that is still
/// coming up must not be reported as serving players, and it must **not** be
/// reported terminal either, or W would release its port out from under it.
pub fn unit_state(host: &mut dyn Host, unit: &str) -> State {
    let out = host.run("systemctl", &["show", unit, "--property=ActiveState", "--value"]);
    match out.unwrap_or_default().trim() {
        "active" => State::Running,
        "activating" | "reloading" => State::Starting,
        "failed" => State::Failed,
        _ => State::Stopped,
    }
}

/// The last 200 journal lines for a unit, oldest first.
fn journal_tail(host: &mut dyn Host, unit: &str) -> Vec<String> {
    let out = host
        .run("journalctl", &["-u", unit, "-n", "200", "--no-pager", "-o", "cat"])
        .unwrap_or_default();
    out.lines().map(str::to_string).take(200).collect()
}

/// Read a server's own status file, tolerating its absence.
///
/// It is written on a timer, so the window before the first write is a normal
/// event on every start rather than an error.
fn read_server_status(path: &Path) -> ServerStatus {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// The box's own numbers, best effort.
///
/// Every one is optional in spirit: a proc file that is not there gives a zero
/// rather than failing the whole report, because the deployment states are the
/// part of this document that matters and losing them over a missing
/// `/proc/loadavg` would be a poor trade.
pub fn box_stats(_host: &mut dyn Host) -> BoxStats {
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let load1 = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    let mem_free_mb = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<u64>()
                .ok()
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0);
    BoxStats { host, load1, mem_free_mb, disk_free_mb: disk_free_mb("/var") }
}

/// Free megabytes on the filesystem holding `path`, via `statvfs`.
fn disk_free_mb(path: &str) -> u64 {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let Ok(c) = CString::new(path) else { return 0 };
        // SAFETY: `statvfs` fills a POD struct; the path is a valid C string
        // and the struct is zeroed before the call.
        unsafe {
            let mut s: libc_statvfs = std::mem::zeroed();
            if statvfs(c.as_ptr(), &mut s) == 0 {
                return s.f_bavail.saturating_mul(s.f_frsize) / (1024 * 1024);
            }
        }
        0
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

// A minimal `statvfs` binding rather than a `libc` dependency for one call.
// The two fields used are at fixed offsets in the glibc struct on the only
// platforms this binary is built for (linux x86_64 and aarch64); everything
// else is padding it never reads.
#[cfg(unix)]
#[repr(C)]
#[derive(Default)]
#[allow(non_camel_case_types)]
struct libc_statvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    __spare: [i32; 6],
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "statvfs64"]
    fn statvfs(path: *const std::ffi::c_char, buf: *mut libc_statvfs) -> i32;
}

/// Sleep between cycles.
pub fn nap(secs: u64) {
    std::thread::sleep(Duration::from_secs(secs.clamp(1, 3600)));
}

/// Where the engine store puts one version.
pub fn engine_dir(root: &Path, version: &str) -> PathBuf {
    root.join("engines").join(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{DeployArgs, Limits};

    /// A `Host` that records what it was asked to do and answers what it is
    /// told to.
    #[derive(Default)]
    struct FakeHost {
        pub calls: Vec<String>,
        pub active: String,
    }

    impl Host for FakeHost {
        fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
            self.calls.push(format!("{prog} {}", args.join(" ")));
            if prog == "systemctl" && args.first() == Some(&"show") {
                return Ok(self.active.clone());
            }
            Ok(String::new())
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("fleet-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn args_in(dir: &Path) -> Args {
        Args {
            root: dir.join("lib"),
            units: dir.join("units"),
            run: dir.join("run"),
            ..Args::default()
        }
    }

    fn dep(id: &str) -> Deployment {
        Deployment {
            deployment_id: id.into(),
            name: "main".into(),
            game: "forgery".into(),
            game_key: None,
            build_id: "b_1".into(),
            build_url: "https://x/y".into(),
            sha256: "aa".into(),
            engine_version: "0.85.0-rc6".into(),
            project: "assets".into(),
            port: 30017,
            args: DeployArgs { scene: Some("scenes/lobby.ron".into()), tick: None, max_players: None },
            limits: Limits::default(),
        }
    }

    /// Put a verified bundle and an engine on the fake box.
    fn seed(a: &Args, digest: &str, version: &str, scene: &str) {
        let b = bundle::dir_for(&a.root, digest);
        std::fs::create_dir_all(b.join("assets").join(Path::new(scene).parent().unwrap())).unwrap();
        std::fs::write(b.join("assets").join(scene), "(nodes: [])").unwrap();
        std::fs::write(
            b.join("floptle-server.ron"),
            format!("(\n  project: \"assets\",\n  scene: {scene:?},\n  engine_version: {version:?},\n)\n"),
        )
        .unwrap();
        std::fs::write(b.join(".verified"), "").unwrap();
        let e = engine_dir(&a.root, version);
        std::fs::create_dir_all(&e).unwrap();
        std::fs::write(e.join("floptle-server"), "#!/bin/true\n").unwrap();
    }

    /// **What the server writes is what the control plane receives.**
    ///
    /// The status file lives at `<run>/<id>/status.json` — inside the
    /// directory the unit declares as its own — and the agent reads it from
    /// there. Before `floptle/0200` the agent read `<run>/<id>.json`, a file
    /// the server could never create, so the report carried a structural zero
    /// for peers and uptime and `null` for the lobby code on every deployment.
    /// Seeded at the new path; a reader still looking at the old one reports
    /// zeros here.
    #[test]
    fn the_report_carries_what_the_server_wrote() {
        let dir = tmp("status");
        let a = args_in(&dir);
        seed(&a, "aa", "0.85.0-rc6", "scenes/lobby.ron");
        std::fs::create_dir_all(a.status_file("d_1").parent().unwrap()).unwrap();
        std::fs::write(
            a.status_file("d_1"),
            "{\"peers\": 3, \"uptime_s\": 812, \"tick_p95_ms\": 4.25, \"lobby_code\": \"UE44B4\", \
             \"port\": null, \"relay\": \"us-east.relay.fopull.com:7788\"}",
        )
        .unwrap();
        let mut host = FakeHost { active: "active".into(), ..Default::default() };
        let mut agent = Agent::default();
        let r = agent.cycle(&a, &mut host, &Desired { deployments: vec![dep("d_1")] }).unwrap();
        let s = &r.deployments[0];
        assert_eq!(s.peers, 3, "peers come from the status file");
        assert_eq!(s.uptime_s, 812, "and uptime — W trusts the reported values only once this is non-zero");
        assert_eq!(s.lobby_code.as_deref(), Some("UE44B4"), "the code is a startup fact a log tail cannot carry");
        assert_eq!(s.tick_p95_ms, Some(4.25));

        // **Where it is reachable is forwarded, not merely read**
        // (`floptle/0212`). The server has reported this since 0.86.2 and the
        // agent dropped it on the floor, so the fix reached an operator on the
        // box and never reached the product — which is the same shape as the
        // lobby code before it, one layer further out.
        assert_eq!(s.port, None, "a relayed server binds nothing, and `null` is the ANSWER");
        assert_eq!(s.relay.as_deref(), Some("us-east.relay.fopull.com:7788"));

        // …and it survives serialization, because the POST body is the only
        // part of this the control plane ever sees. `port` is deliberately
        // asserted ABSENT rather than zero: a control plane that read a
        // missing port as 0 would publish `quic://host:0`.
        let body = r.to_json();
        let d = &body["deployments"][0];
        assert!(d.get("port").is_none(), "a null port must not become a zero one: {d}");
        assert_eq!(d["relay"], "us-east.relay.fopull.com:7788", "{d}");

        // A directly-hosted server reports the port it really bound.
        std::fs::write(
            a.status_file("d_1"),
            "{\"peers\": 0, \"uptime_s\": 5, \"port\": 30000, \"relay\": null}",
        )
        .unwrap();
        let r = agent.cycle(&a, &mut host, &Desired { deployments: vec![dep("d_1")] }).unwrap();
        assert_eq!(r.deployments[0].port, Some(30000));
        assert_eq!(r.deployments[0].relay, None);
        let body = r.to_json();
        assert_eq!(body["deployments"][0]["port"], 30000);
    }

    /// **A deployment that leaves `/desired` is stopped AND reported gone.**
    ///
    /// The report is the half that is easy to leave out and expensive to:
    /// W holds the UDP port for five minutes after a terminal state, and the
    /// clock starts when this agent says the process is gone. Stopping the unit
    /// without reporting leaks the port with nothing to notice it.
    #[test]
    fn a_deployment_that_leaves_desired_is_stopped_and_reported_gone() {
        let dir = tmp("remove");
        let a = args_in(&dir);
        std::fs::create_dir_all(&a.units).unwrap();
        std::fs::write(a.units.join("floptle-d-d_9.service"), "old").unwrap();
        // Something else on the box, which must survive.
        std::fs::write(a.units.join("sshd.service"), "not ours").unwrap();

        let mut h = FakeHost::default();
        let mut agent = Agent::default();
        let report = agent.cycle(&a, &mut h, &Desired::default()).expect("a cycle");

        assert!(
            h.calls.iter().any(|c| c.contains("disable --now floptle-d-d_9.service")),
            "the unit was not stopped: {:?}",
            h.calls
        );
        assert!(!a.units.join("floptle-d-d_9.service").exists(), "and not removed");
        assert!(a.units.join("sshd.service").exists(), "the agent does not own the box");

        let gone = report.deployments.iter().find(|d| d.deployment_id == "d_9").expect("reported");
        assert_eq!(gone.state, "stopped", "the port is held until this state is REPORTED");
        assert!(State::Stopped.is_terminal());
    }

    /// **A unit whose text changed is RESTARTED, not merely started.**
    ///
    /// `systemctl enable --now` on a unit that is already active does nothing:
    /// `--now` means "start it if it is not running", and it was running. So
    /// the agent could write a corrected unit file, log that it had started it,
    /// and leave the old process running the old command line forever.
    ///
    /// That is not hypothetical — it is what happened on `us-east-1` the first
    /// time `floptle/0200`'s fix reached the box. The unit file on disk had the
    /// new `RuntimeDirectory=` and the new `--status-file`; the process was a
    /// day-old one still writing to the path that never worked, so the status
    /// file was still missing and the fix looked like it had failed.
    #[test]
    fn a_unit_whose_text_changed_is_restarted_rather_than_merely_started() {
        let dir = tmp("rewrite");
        let a = args_in(&dir);
        seed(&a, "aa", "0.85.0-rc6", "scenes/lobby.ron");
        let mut h = FakeHost { active: "active".into(), ..Default::default() };
        let mut agent = Agent::default();

        agent.cycle(&a, &mut h, &Desired { deployments: vec![dep("d_1")] }).expect("first cycle");
        let unit_path = a.units.join(unit::unit_name("d_1"));
        let first = std::fs::read_to_string(&unit_path).expect("the first cycle wrote a unit");
        assert!(first.contains("--port 30017"), "{first}");
        h.calls.clear();

        // The same deployment, moved to another port: the unit text changes, so
        // the running process is serving the wrong one until it is replaced.
        let mut moved = dep("d_1");
        moved.port = 30099;
        agent.cycle(&a, &mut h, &Desired { deployments: vec![moved] }).expect("second cycle");

        let second = std::fs::read_to_string(&unit_path).expect("still a unit");
        assert!(second.contains("--port 30099"), "the rewrite never happened:\n{second}");
        assert!(
            h.calls.iter().any(|c| c == "systemctl restart floptle-d-d_1.service"),
            "the unit was rewritten and the old process was left running: {:?}",
            h.calls
        );
    }

    /// A deployment that is already correct is left alone.
    ///
    /// Rewriting the unit and reloading on every ten-second poll would bounce
    /// every server on the box forever — a bug whose symptom is "players get
    /// dropped every ten seconds" and whose cause is nowhere near the netcode.
    #[test]
    fn an_unchanged_deployment_is_not_restarted_every_poll() {
        let dir = tmp("stable");
        let a = args_in(&dir);
        seed(&a, "aa", "0.85.0-rc6", "scenes/lobby.ron");
        let mut h = FakeHost { active: "active".into(), ..Default::default() };
        let mut agent = Agent::default();
        let d = Desired { deployments: vec![dep("d_1")] };

        agent.cycle(&a, &mut h, &d).expect("first cycle writes it");
        assert!(h.calls.iter().any(|c| c.contains("enable")), "first cycle enables it");
        assert!(h.calls.iter().any(|c| c.contains("restart")), "and starts it");

        h.calls.clear();
        let r = agent.cycle(&a, &mut h, &d).expect("second cycle");
        assert!(
            !h.calls.iter().any(|c| {
                c.contains("daemon-reload") || c.contains("enable") || c.contains("restart")
            }),
            "an unchanged deployment was bounced: {:?}",
            h.calls
        );
        assert_eq!(r.deployments[0].state, "running");
    }

    /// **A bundle whose scene is missing is refused before it is started.**
    ///
    /// Otherwise the server starts, fails, restarts and fails, and every one of
    /// those is a page saying "crashed" with nothing a developer can act on.
    /// The reason travels in `last_lines`, which is what the portal shows.
    #[test]
    fn a_bundle_missing_its_scene_fails_with_a_reason_rather_than_crash_looping() {
        let dir = tmp("noscene");
        let a = args_in(&dir);
        seed(&a, "aa", "0.85.0-rc6", "scenes/lobby.ron");
        // The manifest names a scene the bundle does not carry.
        let b = bundle::dir_for(&a.root, "aa");
        std::fs::write(
            b.join("floptle-server.ron"),
            "(\n  project: \"assets\",\n  scene: \"scenes/gone.ron\",\n)\n",
        )
        .unwrap();

        let mut h = FakeHost::default();
        let mut agent = Agent::default();
        let r = agent.cycle(&a, &mut h, &Desired { deployments: vec![dep("d_1")] }).unwrap();
        assert_eq!(r.deployments[0].state, "failed");
        assert!(
            r.deployments[0].last_lines.iter().any(|l| l.contains("gone.ron")),
            "the reason names the scene: {:?}",
            r.deployments[0].last_lines
        );
        assert!(
            !h.calls.iter().any(|c| c.contains("enable --now")),
            "it must not have been started at all"
        );
    }

    /// The bundle's manifest wins over a stale row.
    #[test]
    fn the_bundles_own_manifest_decides_the_scene() {
        let dir = tmp("manifest");
        let a = args_in(&dir);
        seed(&a, "aa", "0.85.0-rc6", "scenes/lobby.ron");
        let mut h = FakeHost { active: "active".into(), ..Default::default() };
        let mut agent = Agent::default();
        // The row says mp.ron; the bundle says lobby.ron. The bundle is what
        // was uploaded, so it is what runs.
        let mut d = dep("d_1");
        d.args.scene = Some("scenes/mp.ron".into());
        agent.cycle(&a, &mut h, &Desired { deployments: vec![d] }).expect("a cycle");
        let unit = std::fs::read_to_string(a.units.join("floptle-d-d_1.service")).unwrap();
        assert!(unit.contains("--scene scenes/lobby.ron"), "{unit}");
        assert!(!unit.contains("mp.ron"), "a stale row must not pick the scene: {unit}");
    }

    /// `activating` is `starting`, and `starting` does not release a port.
    #[test]
    fn a_unit_still_coming_up_is_starting_and_not_terminal() {
        let mut h = FakeHost { active: "activating".into(), ..Default::default() };
        assert_eq!(unit_state(&mut h, "x.service"), State::Starting);
        assert!(!unit_state(&mut h, "x.service").is_terminal());
        h.active = "active".into();
        assert_eq!(unit_state(&mut h, "x.service"), State::Running);
        h.active = "failed".into();
        assert_eq!(unit_state(&mut h, "x.service"), State::Failed);
        h.active = "inactive".into();
        assert_eq!(unit_state(&mut h, "x.service"), State::Stopped);
    }

    /// `--dry-run` touches nothing.
    #[test]
    fn dry_run_writes_no_unit_and_starts_nothing() {
        let dir = tmp("dry");
        let mut a = args_in(&dir);
        a.dry_run = true;
        seed(&a, "aa", "0.85.0-rc6", "scenes/lobby.ron");
        let mut h = FakeHost::default();
        let mut agent = Agent::default();
        agent.cycle(&a, &mut h, &Desired { deployments: vec![dep("d_1")] }).expect("a cycle");
        assert!(!a.units.join("floptle-d-d_1.service").exists(), "a unit was written");
        assert!(h.calls.is_empty() || !h.calls.iter().any(|c| c.contains("enable")), "{:?}", h.calls);
    }
}
