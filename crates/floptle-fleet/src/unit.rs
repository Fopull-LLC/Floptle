//! One systemd unit per deployment, and the reconcile that keeps the set right.
//!
//! ## Why systemd rather than child processes
//!
//! `floptle/0197` asks for "a separate unprivileged systemd unit per deployment
//! with the plan's memory cap". That is not only a packaging preference: the
//! memory cap, the CPU quota and the restart backoff are all things systemd
//! already does correctly, per-unit, in the kernel — and a cap the agent
//! enforced itself would be a cap that vanished the moment the agent was
//! restarted. It also means a deployment survives an agent upgrade: the servers
//! keep running while the thing that reconciles them is replaced.
//!
//! The agent's job is therefore reconciliation and reporting, not supervision.
//!
//! ## The unit is regenerated, never edited
//!
//! Units live in their own directory and are named from the deployment id, so
//! the whole set the agent owns is exactly `floptle-d-*.service` in that
//! directory. A deployment that leaves `/desired` has its unit stopped,
//! disabled and removed; nothing else in the directory is touched.

use std::path::{Path, PathBuf};

use crate::wire::Deployment;

/// The unit name for a deployment.
///
/// The `floptle-d-` prefix is what makes "everything this agent owns" a
/// question with an answer — a reconcile that could not enumerate its own units
/// could only ever add, never remove, and a stopped deployment would run
/// forever.
pub fn unit_name(deployment_id: &str) -> String {
    format!("floptle-d-{}.service", sanitize(deployment_id))
}

/// A deployment id as it may appear in a unit name and a path.
///
/// The control plane's ids are `d_1`-shaped, but this is a value from the
/// network being turned into a **filename the agent then asks systemd to
/// execute**, so it is filtered rather than trusted: anything that is not
/// alphanumeric, `-` or `_` becomes `_`. A `../` in a deployment id would
/// otherwise be a unit written somewhere else entirely.
pub fn sanitize(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    // Never empty, and never a dotfile.
    if s.is_empty() { "unnamed".into() } else { s }
}

/// Everything one unit needs to be written.
pub struct UnitPlan<'a> {
    pub dep: &'a Deployment,
    /// The `floptle-server` for the pinned engine version.
    pub server_bin: PathBuf,
    /// The unpacked bundle's directory.
    pub bundle_dir: PathBuf,
    /// Where this deployment's `--status-file` goes.
    pub status_file: PathBuf,
    /// The scene to host, already reconciled between `/desired` and the
    /// bundle's own manifest.
    pub scene: Option<String>,
    /// The relay this server hosts through, if the region names one.
    pub relay: Option<String>,
    /// The `RuntimeDirectory=` this unit declares, relative to `/run`, so that
    /// `status_file` is in a directory the server's own dynamic user owns.
    /// `None` only when `--run` is somewhere systemd cannot create it.
    pub runtime_dir: Option<String>,
}

/// Render the unit file.
///
/// **`game_key` goes in through the environment, never the command line.**
/// A key on an `ExecStart` is readable by every `ps` on the box and is copied
/// into the journal by systemd's own "Starting…" line — which this agent then
/// ships to the control plane as `last_lines` and W renders on a web page. The
/// same reasoning the relay applies to `--token-file`.
pub fn render(plan: &UnitPlan<'_>) -> String {
    let d = plan.dep;
    let mut exec = format!(
        "{} {} --port {}",
        shell_quote(&plan.server_bin.to_string_lossy()),
        shell_quote(&plan.bundle_dir.join(&d.project).to_string_lossy()),
        d.port
    );
    if let Some(scene) = &plan.scene {
        exec.push_str(&format!(" --scene {}", shell_quote(scene)));
    }
    if let Some(t) = d.args.tick {
        exec.push_str(&format!(" --tick {t}"));
    }
    if let Some(m) = d.args.max_players {
        exec.push_str(&format!(" --max-players {m}"));
    }
    if let Some(r) = &plan.relay {
        exec.push_str(&format!(" --relay {}", shell_quote(r)));
    }
    exec.push_str(&format!(" --status-file {}", shell_quote(&plan.status_file.to_string_lossy())));

    let mut s = String::new();
    s.push_str("# Written by floptle-fleet. Edits are lost on the next reconcile.\n");
    s.push_str("[Unit]\n");
    s.push_str(&format!("Description=Floptle dedicated server {} ({})\n", d.deployment_id, d.game));
    s.push_str("After=network-online.target\n");
    s.push_str("Wants=network-online.target\n");
    // **`[Unit]`, not `[Service]`.** These two are unit-level keys; in
    // `[Service]` systemd logs "Unknown key name … ignoring" and the
    // give-up-after-five-starts promised below never happens, so a build that
    // cannot start restarts forever and the journal fills with one traceback
    // (`floptle/0200`, defect three). The guard checks the SECTION.
    s.push_str("StartLimitIntervalSec=300\nStartLimitBurst=5\n\n");

    s.push_str("[Service]\n");
    s.push_str("Type=simple\n");
    s.push_str(&format!("ExecStart={exec}\n"));
    // The key, out of the command line and out of the journal.
    if let Some(k) = &d.game_key {
        s.push_str(&format!("Environment=FLOPTLE_GAME_KEY={}\n", systemd_escape(k)));
    }
    // **Which deployment this process is.** The server has never been told its
    // own id: it knows a port, a project and a key, and every one of those can
    // be shared or reused. An operator reading a journal, or a crash report
    // arriving at the portal, could not say which row on the page it came from
    // without working backwards through the unit name.
    s.push_str(&format!(
        "Environment=FLOPTLE_DEPLOYMENT_ID={}\n",
        systemd_escape(&d.deployment_id)
    ));
    // ⚠ **The lobby code, when the control plane has one to give — and today it
    // never does.** The relay mints codes (`floptle/0216`), so this is empty on
    // every deployment now and the variable is simply not written. It is here
    // because the agent is the only thing positioned to carry a code from
    // `/desired` to the process, and wiring that after the fact would mean
    // touching the unit renderer during whatever release lands scale-to-zero.
    // **Nothing sets it and nothing reads it yet**; it is a socket, not a
    // feature.
    if let Some(code) = &d.lobby_code {
        s.push_str(&format!("Environment=FLOPTLE_LOBBY_CODE={}\n", systemd_escape(code)));
    }
    // stdout is the log, and the journal is where it goes — `floptle/0197`
    // asks for no file logging on the box.
    s.push_str("StandardOutput=journal\nStandardError=journal\n");

    // **The caps, and only when the plan actually set them.** A zero here would
    // be a unit systemd kills on its first allocation; see `wire::Limits`.
    if let Some(mb) = d.limits.memory_max() {
        s.push_str(&format!("MemoryMax={mb}M\n"));
    }
    if let Some(pct) = d.limits.cpu_quota() {
        s.push_str(&format!("CPUQuota={pct}%\n"));
    }

    // Restart with backoff, which is what `floptle/0197` asks for and what
    // systemd does better than a loop in this agent would. The burst limit is
    // deliberately not `always`: a build that cannot start must eventually stop
    // trying and sit in `failed`, where the agent reports it and a developer
    // sees a reason, rather than restarting forever and filling the journal
    // with the same traceback.
    s.push_str("Restart=on-failure\nRestartSec=5\n");

    // Unprivileged, and confined to what a game server needs.
    s.push_str("DynamicUser=yes\n");
    s.push_str(&format!("StateDirectory=floptle-fleet/{}\n", sanitize(&d.deployment_id)));
    // **The directory the status file is in is this unit's own.** A dynamic
    // user under `ProtectSystem=strict` can write nowhere it is not handed,
    // and the agent's root-owned runtime directory was not such a place — so
    // the file was never written and every deployment reported zeros forever
    // (`floptle/0200`, defect one). systemd creates this one owned by the
    // server's user; see `Args::runtime_directory` for why it is not nested
    // under the agent's.
    if let Some(rd) = &plan.runtime_dir {
        s.push_str(&format!("RuntimeDirectory={rd}\n"));
    }
    s.push_str("NoNewPrivileges=yes\n");
    s.push_str("PrivateTmp=yes\n");
    s.push_str("PrivateDevices=yes\n");
    s.push_str("ProtectSystem=strict\n");
    s.push_str("ProtectHome=yes\n");
    s.push_str("ProtectKernelTunables=yes\n");
    s.push_str("ProtectKernelModules=yes\n");
    s.push_str("ProtectControlGroups=yes\n");
    s.push_str("RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX\n");
    s.push_str("RestrictNamespaces=yes\n");
    s.push_str("LockPersonality=yes\n");
    s.push_str("MemoryDenyWriteExecute=no\n");
    s.push_str("SystemCallFilter=@system-service\n");
    // **A game server runs a developer's Lua, on a box the developer does not
    // own.** What a script can reach inside the process is the engine's job
    // (`floptle_script::http_policy`); what the PROCESS can reach is this
    // unit's. Link-local is where a cloud box's metadata service answers, and
    // there is nothing at that address a game server has any business with.
    // Not loopback or the private ranges: the relay may share the box.
    s.push_str("IPAddressDeny=link-local multicast\n");
    // The status file names the lobby code and the key's prefix; the state
    // directory holds saves. Neither is another dynamic user's to read.
    s.push_str("RuntimeDirectoryMode=0750\n");
    s.push_str("StateDirectoryMode=0750\n");
    s.push_str("UMask=0077\n");
    s.push_str("ProtectProc=invisible\n");
    s.push_str("ProcSubset=pid\n");
    s.push_str("RestrictSUIDSGID=yes\n");
    s.push_str("RemoveIPC=yes\n");
    s.push_str("CapabilityBoundingSet=\n");
    s.push_str("SystemCallArchitectures=native\n");
    s.push_str("RestrictRealtime=yes\n");
    s.push_str("TasksMax=256\n");
    s.push_str("LimitNOFILE=4096\n");
    s.push('\n');

    s.push_str("[Install]\nWantedBy=multi-user.target\n");
    s
}

/// Quote a value for an `ExecStart` line.
///
/// systemd splits `ExecStart` on whitespace itself, so a path with a space in
/// it becomes two arguments and the server is started against a directory that
/// does not exist. Double quotes are systemd's own quoting.
///
/// **Control characters are dropped, not quoted.** A unit file is lines, and no
/// quoting makes a newline safe inside one — a value that carries one would
/// close the `ExecStart=` and open whatever directive followed. The agent
/// refuses such a row before it gets here (`Deployment::refuse_unsafe`); this
/// is the second lock on the same door.
pub fn shell_quote(s: &str) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=".contains(c)) {
        s
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Escape a value for `Environment=`, which is one line and cannot hold one.
fn systemd_escape(s: &str) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control()).collect();
    if clean.contains(' ') { format!("\"{clean}\"") } else { clean }
}

/// The unit files this agent owns in `dir`, by unit name.
pub fn owned_units(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("floptle-d-") && name.ends_with(".service") {
            out.push(name);
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{DeployArgs, Limits};

    fn dep() -> Deployment {
        Deployment {
            deployment_id: "d_1".into(),
            name: "main".into(),
            game: "forgery".into(),
            game_key: Some("fk_live_secret".into()),
            build_id: "b_1".into(),
            build_url: "https://fopull.com/x".into(),
            sha256: "aa".into(),
            engine_version: "0.85.0-rc6".into(),
            project: "assets".into(),
            port: 30017,
            lobby_code: None,
            args: DeployArgs {
                scene: Some("scenes/lobby.ron".into()),
                tick: Some(60.0),
                max_players: Some(20),
            },
            limits: Limits { cpu_quota_pct: 100, memory_max_mb: 1024 },
        }
    }

    fn plan_for(d: &Deployment) -> UnitPlan<'_> {
        UnitPlan {
            dep: d,
            server_bin: PathBuf::from("/var/lib/floptle-fleet/engines/0.85.0-rc6/floptle-server"),
            bundle_dir: PathBuf::from("/var/lib/floptle-fleet/builds/aa"),
            status_file: PathBuf::from("/run/floptle-d/d_1/status.json"),
            scene: Some("scenes/lobby.ron".into()),
            relay: Some("relay.fopull.com:7788".into()),
            runtime_dir: Some("floptle-d/d_1".into()),
        }
    }

    /// The lines of one `[Section]` of a unit.
    fn section<'a>(unit: &'a str, name: &str) -> Vec<&'a str> {
        let head = format!("[{name}]");
        unit.lines()
            .skip_while(|l| *l != head)
            .skip(1)
            .take_while(|l| !l.starts_with('['))
            .collect()
    }

    /// **The status file is in a directory the server's own user owns.**
    ///
    /// `floptle/0200`, defect one: the server runs `DynamicUser=yes` under
    /// `ProtectSystem=strict`, and the file used to be pointed at the AGENT's
    /// runtime directory — root-owned, `0755` — so it was never written, and
    /// every deployment reported zero players, zero uptime and no lobby code
    /// forever. The portal showed a running server nobody could join. The unit
    /// now declares its own `RuntimeDirectory=`, which systemd creates owned by
    /// the dynamic user, and `--status-file` points inside it.
    #[test]
    fn the_status_file_is_in_a_directory_the_server_can_write() {
        let d = dep();
        let u = render(&plan_for(&d));
        let service = section(&u, "Service");
        assert!(
            service.contains(&"RuntimeDirectory=floptle-d/d_1"),
            "the unit must own the directory its status file is in:\n{u}"
        );
        let exec = u.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        assert!(
            exec.contains("--status-file /run/floptle-d/d_1/status.json"),
            "and point the status file inside it: {exec}"
        );
    }

    /// **`StartLimitIntervalSec` and `StartLimitBurst` are `[Unit]` keys.**
    ///
    /// `floptle/0200`, defect three: in `[Service]` systemd logs "Unknown key
    /// name … ignoring" and the documented give-up-after-five-starts never
    /// happens, so a build that cannot start restarts forever, writing the
    /// same traceback into a journal the agent then ships every ten seconds.
    /// Asserted by SECTION rather than by substring, because the substring was
    /// there all along.
    #[test]
    fn the_start_limit_is_where_systemd_reads_it() {
        let d = dep();
        let u = render(&plan_for(&d));
        let unit = section(&u, "Unit");
        let service = section(&u, "Service");
        for key in ["StartLimitIntervalSec=300", "StartLimitBurst=5"] {
            assert!(unit.contains(&key), "{key} belongs in [Unit], where systemd reads it:\n{u}");
            assert!(!service.contains(&key), "{key} in [Service] is ignored with a warning:\n{u}");
        }
    }

    /// **The game key never reaches the command line.**
    ///
    /// It is a credential for somebody else's game, and an `ExecStart` is
    /// readable by every `ps` on the box AND echoed into the journal by
    /// systemd — which this agent ships to the control plane as `last_lines`
    /// and W renders on a public page. Three ways out of the box for one
    /// mistake, so this is asserted rather than reviewed.
    #[test]
    fn the_game_key_is_in_the_environment_and_not_the_command_line() {
        let d = dep();
        let u = render(&plan_for(&d));
        let exec = u.lines().find(|l| l.starts_with("ExecStart=")).expect("an ExecStart");
        assert!(
            !exec.contains("fk_live_secret"),
            "the key is in the command line, where `ps` and the journal both read it: {exec}"
        );
        assert!(u.contains("Environment=FLOPTLE_GAME_KEY=fk_live_secret"), "{u}");
    }

    /// **A server is told which deployment it is, and its lobby code when there
    /// is one to tell it.**
    ///
    /// The id is unconditional: a process that knows only a port, a project and
    /// a key knows three things that can each be shared or reused, so nothing
    /// in its journal could be traced back to a row on the portal.
    ///
    /// ⚠ The code is written **only when `/desired` carried one**, and today it
    /// never does — the relay mints codes, not the control plane
    /// (`floptle/0216`). An empty `Environment=FLOPTLE_LOBBY_CODE=` would be
    /// worse than the variable being absent: a server reading it would find a
    /// set-but-empty code rather than no code, which is the same
    /// zero-versus-unmeasured confusion that bit the box stats.
    #[test]
    fn a_unit_names_its_deployment_and_omits_a_code_it_was_not_given() {
        let d = dep();
        let u = render(&plan_for(&d));
        assert!(u.contains("Environment=FLOPTLE_DEPLOYMENT_ID=d_1"), "{u}");
        assert!(
            !u.contains("FLOPTLE_LOBBY_CODE"),
            "an unset code was written as an empty variable: {u}"
        );

        let with_code = Deployment { lobby_code: Some("U5FEFJ".into()), ..dep() };
        let u = render(&plan_for(&with_code));
        assert!(u.contains("Environment=FLOPTLE_LOBBY_CODE=U5FEFJ"), "{u}");
    }

    /// The unit says what to run, where, and on which port — and the scene and
    /// port are the two a wrong answer makes into a crash-loop.
    #[test]
    fn the_unit_runs_the_pinned_engine_against_the_bundles_project() {
        let d = dep();
        let u = render(&plan_for(&d));
        let exec = u.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        assert!(exec.contains("/engines/0.85.0-rc6/floptle-server"), "the PINNED engine: {exec}");
        assert!(exec.contains("/builds/aa/assets"), "the project INSIDE the bundle: {exec}");
        assert!(exec.contains("--port 30017"), "{exec}");
        assert!(exec.contains("--scene scenes/lobby.ron"), "{exec}");
        assert!(exec.contains("--max-players 20"), "{exec}");
        assert!(exec.contains("--relay relay.fopull.com:7788"), "hosts through the relay: {exec}");
        assert!(exec.contains("--status-file"), "or the agent has no peer count: {exec}");
    }

    /// **Zero limits write no cap at all.**
    ///
    /// This is what production answers today. `MemoryMax=0M` is a unit the
    /// kernel kills on its first allocation, so a good build would crash-loop
    /// and the portal would report the build as broken.
    #[test]
    fn todays_zero_limits_write_no_cap() {
        let mut d = dep();
        d.limits = Limits { cpu_quota_pct: 0, memory_max_mb: 0 };
        let u = render(&plan_for(&d));
        assert!(!u.contains("MemoryMax"), "a cap of zero is not a cap:\n{u}");
        assert!(!u.contains("CPUQuota"), "nor is a quota of zero:\n{u}");
        // …and a real plan does write them.
        let d = dep();
        let u = render(&plan_for(&d));
        assert!(u.contains("MemoryMax=1024M"), "{u}");
        assert!(u.contains("CPUQuota=100%"), "{u}");
    }

    /// A deployment id is a value off the network that becomes a filename the
    /// agent asks systemd to execute. It is filtered, not trusted.
    #[test]
    fn a_deployment_id_cannot_write_a_unit_somewhere_else() {
        assert_eq!(unit_name("d_1"), "floptle-d-d_1.service");
        assert_eq!(unit_name("../../etc/systemd/system/evil"), "floptle-d-______etc_systemd_system_evil.service");
        assert!(!unit_name("a/b").contains('/'), "a path separator would escape the unit dir");
        assert_eq!(sanitize(""), "unnamed");
    }

    /// **The process cannot reach the box's metadata service, and its files
    /// are its own.** Asserted by SECTION, the lesson of the start-limit keys:
    /// a directive in the wrong section is logged and ignored.
    #[test]
    fn the_unit_denies_link_local_and_keeps_its_directories_private() {
        let d = dep();
        let u = render(&plan_for(&d));
        let service = section(&u, "Service");
        for key in [
            "IPAddressDeny=link-local multicast",
            "RuntimeDirectoryMode=0750",
            "StateDirectoryMode=0750",
            "UMask=0077",
            "ProtectProc=invisible",
            "ProcSubset=pid",
            "RestrictSUIDSGID=yes",
            "RemoveIPC=yes",
            "CapabilityBoundingSet=",
            "SystemCallArchitectures=native",
            "RestrictRealtime=yes",
            "TasksMax=256",
            "LimitNOFILE=4096",
        ] {
            assert!(service.contains(&key), "{key} is missing from [Service]:\n{u}");
        }
        // Not loopback and not the private ranges: the relay may share the box.
        assert!(!u.contains("IPAddressDeny=any"), "{u}");
    }

    /// **A newline in a quoted value is not a quoted newline; it is a new
    /// directive.** The agent refuses such a row first; the quoter drops the
    /// character regardless, so the unit stays one `ExecStart=` and no
    /// `User=` whatever reaches it.
    #[test]
    fn a_control_character_in_a_value_cannot_add_a_directive() {
        assert_eq!(shell_quote("a\nUser=root"), "aUser=root");
        assert_eq!(shell_quote("scenes/x.ron\r\n"), "scenes/x.ron");
        assert_eq!(shell_quote("tab\there"), "tabhere");
        assert_eq!(shell_quote("a b\n"), "\"a b\"");
        let mut d = dep();
        d.args.scene = Some("arena\nUser=root".into());
        let mut plan = plan_for(&d);
        plan.scene = d.args.scene.clone();
        let u = render(&plan);
        assert_eq!(u.lines().filter(|l| l.starts_with("ExecStart=")).count(), 1, "{u}");
        assert!(!u.lines().any(|l| l.starts_with("User=")), "{u}");
    }

    /// The unit restarts a crash, and eventually stops trying.
    ///
    /// The second half is the one worth stating: `Restart=always` with no burst
    /// limit turns a build that cannot start into a machine writing the same
    /// traceback forever, and the deployment never reaches `failed`, so nobody
    /// is ever told.
    #[test]
    fn a_crash_restarts_with_backoff_and_eventually_gives_up() {
        let d = dep();
        let u = render(&plan_for(&d));
        assert!(u.contains("Restart=on-failure"), "{u}");
        assert!(u.contains("RestartSec=5"), "{u}");
        assert!(u.contains("StartLimitBurst="), "a restart loop with no ceiling never reports: {u}");
        assert!(u.contains("DynamicUser=yes"), "{u}");
        assert!(u.contains("NoNewPrivileges=yes"), "{u}");
    }

    /// A path with a space in it stays one argument.
    #[test]
    fn a_path_with_a_space_is_one_argument() {
        assert_eq!(shell_quote("/var/lib/x"), "/var/lib/x");
        assert_eq!(shell_quote("/var/my games/x"), "\"/var/my games/x\"");
        let d = dep();
        let mut p = plan_for(&d);
        p.bundle_dir = PathBuf::from("/var/lib/floptle fleet/builds/aa");
        let u = render(&p);
        let exec = u.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        assert!(exec.contains("\"/var/lib/floptle fleet/builds/aa/assets\""), "{exec}");
    }

    /// The agent can enumerate exactly the units it owns, which is what makes
    /// removing a deployment possible at all.
    #[test]
    fn the_agent_can_name_its_own_units_and_only_its_own() {
        let dir = std::env::temp_dir().join(format!("fleet-units-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["floptle-d-d_1.service", "floptle-d-d_2.service", "sshd.service", "notes.txt"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let owned = owned_units(&dir);
        assert_eq!(owned, vec!["floptle-d-d_1.service", "floptle-d-d_2.service"]);
        assert!(!owned.iter().any(|u| u.contains("sshd")), "the agent does not own the box");
    }
}
