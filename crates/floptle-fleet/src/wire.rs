//! What the control plane says, and what this box says back (`floptle/0199` §3).
//!
//! These types are written against the **live** payloads W generated from a real
//! production row, not against the §6 draft — the draft is missing three fields
//! the agent cannot work without (`engine_version`, `project`, `sha256`).
//!
//! **Every field the agent does not strictly need is optional**, and unknown
//! fields are ignored rather than refused. A control plane that adds a field
//! must never stop a box that has not been updated yet: the failure mode of a
//! strict parse here is an entire region going dark on a deploy of the website.

use serde::{Deserialize, Serialize};

/// `GET /api/floptle/v1/cloud/fleet/{region}/desired`
#[derive(Debug, Default, Deserialize)]
pub struct Desired {
    #[serde(default)]
    pub deployments: Vec<Deployment>,
}

/// One deployment the region is supposed to be running.
#[derive(Debug, Clone, Deserialize)]
pub struct Deployment {
    pub deployment_id: String,
    /// The deployment's display name ("main"). Carried so the journal and any
    /// future message can name it the way the portal does.
    #[serde(default)]
    #[allow(dead_code)]
    pub name: String,
    #[serde(default)]
    pub game: String,
    /// The game key this server presents. Recorded, never logged — see
    /// [`Deployment::redacted`].
    #[serde(default)]
    pub game_key: Option<String>,
    #[serde(default)]
    pub build_id: String,
    /// **Signed, and it expires in sixty minutes.** Fetched fresh from
    /// `/desired` every cycle and never stored, which is also why the bundle
    /// cache is keyed on the DIGEST rather than on this.
    pub build_url: String,
    /// The digest the downloaded bytes must have before anything unpacks or
    /// runs them.
    pub sha256: String,
    /// Which `floptle-server` runs it.
    pub engine_version: String,
    /// The project directory INSIDE the bundle (`"assets"`).
    #[serde(default = "default_project")]
    pub project: String,
    pub port: u16,
    /// The lobby code this deployment should present, when the control plane
    /// allocates one. ⚠ **Absent on every deployment today** — the relay mints
    /// codes, not the control plane (`floptle/0216`). Read here so the agent
    /// can carry one the day that inverts, and ignored until then.
    #[serde(default)]
    pub lobby_code: Option<String>,
    #[serde(default)]
    pub args: DeployArgs,
    #[serde(default)]
    pub limits: Limits,
}

fn default_project() -> String {
    "assets".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeployArgs {
    #[serde(default)]
    pub scene: Option<String>,
    #[serde(default)]
    pub tick: Option<f32>,
    #[serde(default)]
    pub max_players: Option<u32>,
}

/// The plan's caps.
///
/// ⚠ **Zero means "not entitled yet", not "no memory"** (`floptle/0199` §3).
/// `billing.server_slots_available` is off on production today and zeroes the
/// slot entitlement everywhere by design, so `/desired` currently answers
/// `{"cpu_quota_pct":0,"memory_max_mb":0}`. Writing `MemoryMax=0` into a unit
/// would make systemd kill the server the instant it allocated anything — the
/// deployment would crash-loop, and the page would say "crashed" about a
/// perfectly good build. So zero is read as *unset* and the cap is simply not
/// written; see [`Limits::memory_max`].
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Limits {
    #[serde(default)]
    pub cpu_quota_pct: u32,
    #[serde(default)]
    pub memory_max_mb: u64,
}

impl Limits {
    /// The memory cap to write, or `None` when the control plane has not set
    /// one. Never `Some(0)`.
    pub fn memory_max(&self) -> Option<u64> {
        (self.memory_max_mb > 0).then_some(self.memory_max_mb)
    }

    /// The CPU quota to write, or `None`. Same rule, same reason: a
    /// `CPUQuota=0%` unit gets no CPU at all.
    pub fn cpu_quota(&self) -> Option<u32> {
        (self.cpu_quota_pct > 0).then_some(self.cpu_quota_pct)
    }
}

impl Deployment {
    /// This deployment with its game key removed, for logging.
    ///
    /// The key is a credential for somebody else's game. It arrives on every
    /// poll, and an agent that logged what it received would write it into the
    /// journal ten times a minute — where the last 200 lines are then POSTed
    /// back to the control plane and shown on a web page.
    pub fn redacted(&self) -> String {
        format!(
            "{} ({}) build {} engine {} port {}",
            self.deployment_id, self.game, self.build_id, self.engine_version, self.port
        )
    }

    /// **Every string in a row becomes text in a unit file the agent writes as
    /// root**, and a unit file is lines: a value carrying a newline is a value
    /// that ends one directive and starts another. The control plane validates
    /// what a developer types, and this refuses anyway — a row that fails here
    /// is reported `failed` with the field named, and no unit is written.
    ///
    /// Lengths are capped too: none of these is prose, and a kilobyte of scene
    /// name is not a scene name.
    pub fn refuse_unsafe(&self) -> Result<(), String> {
        let fields: [(&str, Option<&str>, usize); 11] = [
            ("deployment_id", Some(&self.deployment_id), 256),
            ("name", Some(&self.name), 256),
            ("game", Some(&self.game), 256),
            ("game_key", self.game_key.as_deref(), 256),
            ("build_id", Some(&self.build_id), 256),
            // A signed download URL is the one long value a row legitimately
            // carries.
            ("build_url", Some(&self.build_url), 4096),
            ("sha256", Some(&self.sha256), 256),
            ("engine_version", Some(&self.engine_version), 256),
            ("project", Some(&self.project), 256),
            ("lobby_code", self.lobby_code.as_deref(), 256),
            ("args.scene", self.args.scene.as_deref(), 256),
        ];
        for (name, value, max) in fields {
            let Some(v) = value else { continue };
            if v.chars().any(char::is_control) {
                return Err(format!("{name} contains a control character; refused"));
            }
            if v.len() > max {
                return Err(format!("{name} is {} bytes, more than the {max} allowed; refused", v.len()));
            }
        }
        Ok(())
    }
}

/// What a deployment is doing, in the control plane's words.
///
/// The spelling matters: anything outside this set leaves the stored state
/// alone, so a typo here is a deployment that never changes state on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Starting,
    Running,
    Stopped,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Running => "running",
            State::Stopped => "stopped",
            State::Failed => "failed",
        }
    }

    /// **Is this the state that releases the port?**
    ///
    /// W holds a deployment's UDP port for five minutes after a terminal
    /// report, because handing a live port on while the old server's players
    /// are still sending packets delivers their traffic into a different game.
    /// The hold starts when THIS agent says the process is gone — not when the
    /// developer pressed stop — so failing to report a terminal state leaks the
    /// port until somebody notices by hand.
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Stopped | State::Failed)
    }
}

/// `POST /api/floptle/v1/cloud/fleet/{region}/status`
#[derive(Debug, Serialize)]
pub struct Report {
    pub box_: BoxStats,
    pub deployments: Vec<DeploymentStatus>,
}

impl Report {
    /// Serialize with the field actually named `box`.
    ///
    /// `box` is a reserved word in Rust, and `#[serde(rename)]` on the struct
    /// field would be the ordinary fix — this is a hand-built value instead
    /// because it is the whole document and building it once here keeps the
    /// wire shape in one readable place.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "box": self.box_,
            "deployments": self.deployments,
        })
    }
}

/// What this machine says about itself, alongside what its deployments are doing.
///
/// ⚠ **Every measurement is optional, and an unmeasurable one is OMITTED rather
/// than sent as zero** (`floptle/0213`). The control plane reads `mem_free_mb`
/// now, and treats a box that reports little free memory as full regardless of
/// how many slots its declaration still shows — so a `/proc/meminfo` this agent
/// could not read, sent as `0`, is a healthy box declaring itself out of memory.
/// That does not merely stop a placement: a region whose only box looks full is
/// a region the control plane prices a NEW MACHINE for. "Did not measure" and
/// "measured none left" are opposite facts and only one of them should cost
/// money.
///
/// `host` is the exception and is always sent: it is the field `/desired` selects
/// on, and a box that does not name itself is served its region's whole list.
#[derive(Debug, Default, Serialize)]
pub struct BoxStats {
    pub host: String,
    /// **This agent's own version** (`floptle/0232`), the string compiled into
    /// the binary — never read from a file, a unit or a row, so an upgrade
    /// cannot leave it saying the old number. The control plane had one
    /// version field, hand-maintained, and it read `0.86.3` for a box running
    /// `0.89.0`; a new box in the region would have been provisioned to
    /// match it.
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load1: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_free_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_mb: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct DeploymentStatus {
    pub deployment_id: String,
    pub state: &'static str,
    pub peers: u32,
    pub uptime_s: u64,
    pub restarts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tick_p95_ms: Option<f32>,
    /// **What this deployment actually costs in memory**, from systemd's own
    /// cgroup accounting (`floptle/0214`).
    ///
    /// `servers_per_box` is arithmetic on DECLARED quotas — six Studio slots at
    /// 1536 MB inside 11.9 GB — and had never been checked against a running
    /// server. A measured idle one is ~21 MB. Whether a loaded one is 200 MB or
    /// 1500 MB is the difference between a box holding roughly 40 and holding
    /// 6, and nobody could answer it because nothing measured it.
    ///
    /// Cgroup rather than the main process's RSS on purpose: it counts anything
    /// the server forks, and it is the same number the `MemoryMax` cap is
    /// enforced against, so a deployment nearing its limit reads as nearing its
    /// limit rather than as merely large.
    /// **The ceiling the engine is actually enforcing** (`floptle/0221`).
    ///
    /// ⚠ **Third time this seam has been wrong in one direction**: `port` and
    /// `relay` were written by the server and dropped here too (`floptle/0209`,
    /// `floptle/0212`). The server has written `max_players` into its status
    /// file all along and the agent parsed the file without carrying this one
    /// field, so the control plane stored null while the box knew the answer.
    ///
    /// It exists so the two ceilings can be COMPARED. The control plane sets a
    /// cap from the account's plan and the engine enforces one from
    /// `--max-players`; when they disagree a developer meets whichever is lower
    /// with nothing to say which. They agree today — this is a monitor going in
    /// before it is needed.
    ///
    /// ⚠ Absent when the server does not report it, never `0`: a cap of zero is
    /// a server that admits nobody, which is the opposite of "no cap set".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_players: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_mb: Option<u64>,
    /// The **high-water mark since this unit started**, from systemd's
    /// `MemoryPeak`.
    ///
    /// This is the number that sizes a slot: an average tells you what a server
    /// idles at, and a box is sized by what its servers PEAK at. Peak "since
    /// the last report" would have been identical to `mem_mb` — the agent
    /// reports every cycle — so the useful window is the unit's whole life,
    /// which systemd already keeps at no cost.
    ///
    /// Absent on systemd older than v253, which does not expose the property.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_peak_mb: Option<u64>,
    /// The most recent 200 journal lines, oldest first.
    pub last_lines: Vec<String>,
    /// The six-character code players join with, once the server has one.
    ///
    /// Not in the shape W specified — offered because the portal shows a lobby
    /// code and this is the only place the real one exists. An unknown field is
    /// ignored by the endpoint, so sending it costs nothing if W does not want
    /// it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lobby_code: Option<String>,
    /// **Where the server says it is reachable**, straight from its own status
    /// file rather than derived from the port the control plane allocated.
    ///
    /// The control plane was building `quic://<host>:<allocated port>` for every
    /// deployment, and for a relayed one nothing is listening there at all —
    /// for two days that address reached a *different* game, a stray process
    /// that happened to hold the port (`floptle/0209`). The server has known
    /// the answer since 0.86.2 and the agent simply did not carry it, so the
    /// fix reached an operator on the box and not the product
    /// (`floptle/0212`).
    ///
    /// Both are skipped when absent, so a control plane that ignores them —
    /// and an older server that does not report them — are unaffected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// **Which key the running server was started with** — the first twelve
    /// characters, never the key (`floptle/0229`). Shown beside the rotate
    /// control, so a developer mid-rotation can see what the process that
    /// is running presented.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_key_prefix: Option<String>,
}

impl DeploymentStatus {
    /// **The one place the status file becomes the wire** (`floptle/0229`).
    ///
    /// The server writes a field; the agent parses it; the control plane
    /// stores null — four times in one direction (`lobby_code` 0200,
    /// `port`/`relay` 0212, `max_players` 0221, `game_key_prefix` 0229),
    /// each time because the copy from [`ServerStatus`] to this struct was
    /// hand-written at the call site and the new field was not in it. Every
    /// field the file carries is carried here, in one function the report
    /// and the tests both go through; `every_status_file_field_reaches_the_wire`
    /// holds it to that.
    pub fn from_server(
        deployment_id: String,
        state: State,
        s: ServerStatus,
        restarts: u32,
        (mem_mb, mem_peak_mb): (Option<u64>, Option<u64>),
        last_lines: Vec<String>,
    ) -> Self {
        Self {
            deployment_id,
            state: state.as_str(),
            peers: s.peers,
            uptime_s: s.uptime_s,
            restarts,
            tick_p95_ms: s.tick_p95_ms,
            max_players: s.max_players,
            mem_mb,
            mem_peak_mb,
            last_lines,
            lobby_code: s.lobby_code,
            port: s.port,
            relay: s.relay,
            game_key_prefix: s.game_key_prefix,
        }
    }

    /// A deployment the agent could not start, or has stopped: nothing
    /// measured — no cgroup to ask is not the same as no memory used — and
    /// the reason, if any, in `last_lines`.
    pub fn unmeasured(deployment_id: String, state: State, restarts: u32, last_lines: Vec<String>) -> Self {
        Self::from_server(deployment_id, state, ServerStatus::default(), restarts, (None, None), last_lines)
    }
}

/// What `floptle-server --status-file` writes, as much of it as the agent uses.
///
/// Every field is optional: the file is written on a timer, so the agent will
/// routinely read it in the window before the first write, and a half-written
/// one is a normal event rather than an error. It is written temp-file-and-rename
/// on the server's side, so a torn read is not possible — an ABSENT one is.
#[derive(Debug, Default, Deserialize)]
pub struct ServerStatus {
    #[serde(default)]
    pub peers: u32,
    #[serde(default)]
    pub uptime_s: u64,
    #[serde(default)]
    pub tick_p95_ms: Option<f32>,
    #[serde(default)]
    pub lobby_code: Option<String>,
    /// The player ceiling the ENGINE is enforcing, as the server reports it.
    #[serde(default)]
    pub max_players: Option<u32>,
    /// **Where this server is actually reachable** (`floptle/0209`, forwarded
    /// by `floptle/0212`): the UDP port it bound, or `None` when it listens on
    /// nothing because it went out through a relay.
    #[serde(default)]
    pub port: Option<u16>,
    /// The relay it registered with, or `None` when it listens directly.
    #[serde(default)]
    pub relay: Option<String>,
    /// The first twelve characters of the key the server was started with
    /// (`floptle/0229`). The whole key is never in the file.
    #[serde(default)]
    pub game_key_prefix: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The real `/desired` payload parses**, field for field.
    ///
    /// Copied from `floptle/0199` §3, which W generated from the live
    /// production row rather than writing by hand — so this is the one fixture
    /// in the crate that is known to match what the endpoint emits.
    #[test]
    fn the_live_desired_payload_parses() {
        let body = r#"{"deployments":[{
          "deployment_id":"d_1",
          "name":"main",
          "game":"forgery",
          "game_key":"fk_live_abc",
          "build_id":"b_1",
          "build_url":"https://fopull.com/api/floptle/v1/cloud/builds/download/xyz?expires=1&signature=2",
          "sha256":"a8a9d337a5b5e4e213a35123138278d10c0b923e49e336e45c5c55cac9657a31",
          "engine_version":"0.85.0-rc6",
          "project":"assets",
          "port":30017,
          "args":{"scene":"scenes/lobby.ron","tick":60,"max_players":20},
          "limits":{"cpu_quota_pct":100,"memory_max_mb":1024}
        }]}"#;
        let d: Desired = serde_json::from_str(body).expect("the live payload");
        let one = &d.deployments[0];
        assert_eq!(one.deployment_id, "d_1");
        assert_eq!(one.engine_version, "0.85.0-rc6");
        assert_eq!(one.project, "assets");
        assert_eq!(one.port, 30017);
        assert_eq!(one.args.scene.as_deref(), Some("scenes/lobby.ron"));
        assert_eq!(one.args.max_players, Some(20));
        assert_eq!(one.limits.memory_max(), Some(1024));
        assert_eq!(one.limits.cpu_quota(), Some(100));
        // The key is carried and must not appear in what the agent logs.
        assert!(!one.redacted().contains("fk_live_abc"), "{}", one.redacted());
    }

    /// **Zero limits are "not entitled yet", and must not become a cap.**
    ///
    /// This is what production answers TODAY, so it is the payload the agent
    /// will first meet on the box. `MemoryMax=0` is not a large cap or a
    /// missing one — it is a unit systemd kills on its first allocation, so a
    /// good build would crash-loop and the portal would blame the build.
    #[test]
    fn todays_zero_limits_are_unset_rather_than_a_cap_of_zero() {
        let body = r#"{"deployments":[{"deployment_id":"d_1","build_url":"https://x/y",
          "sha256":"aa","engine_version":"0.85.0-rc6","port":30017,
          "limits":{"cpu_quota_pct":0,"memory_max_mb":0}}]}"#;
        let d: Desired = serde_json::from_str(body).expect("parses");
        let l = d.deployments[0].limits;
        assert_eq!(l.memory_max(), None, "0 MB is not a memory cap of zero");
        assert_eq!(l.cpu_quota(), None, "0% is not a CPU quota of zero");
        // …and the fields the draft lacked still have workable defaults.
        assert_eq!(d.deployments[0].project, "assets");
    }

    /// A field the control plane adds tomorrow must not take the region down.
    #[test]
    fn an_unknown_field_is_ignored_rather_than_refused() {
        let body = r#"{"deployments":[{"deployment_id":"d_1","build_url":"https://x/y",
          "sha256":"aa","engine_version":"1","port":1,"something_new":{"a":1}}],
          "also_new":42}"#;
        let d: Desired = serde_json::from_str(body).expect("a new field is not an outage");
        assert_eq!(d.deployments.len(), 1);
    }

    /// The two states that release the port are the two that say the process is
    /// gone — and `starting` is emphatically not one of them.
    #[test]
    fn only_a_process_that_is_gone_releases_its_port() {
        assert!(State::Stopped.is_terminal());
        assert!(State::Failed.is_terminal());
        assert!(!State::Running.is_terminal());
        assert!(!State::Starting.is_terminal(), "a slow start must not free the port");
        // The spellings are the control plane's; anything else silently leaves
        // the stored state alone, so they are asserted rather than trusted.
        assert_eq!(State::Starting.as_str(), "starting");
        assert_eq!(State::Running.as_str(), "running");
        assert_eq!(State::Stopped.as_str(), "stopped");
        assert_eq!(State::Failed.as_str(), "failed");
    }

    /// The report serializes with the field named `box`, which Rust will not
    /// let the struct field be called.
    #[test]
    fn the_status_document_names_the_box() {
        let r = Report {
            box_: BoxStats {
                host: "us-east-1".into(),
                version: "0.89.1".into(),
                load1: Some(0.4),
                mem_free_mb: Some(9000),
                disk_free_mb: Some(40000),
            },
            deployments: vec![DeploymentStatus {
                deployment_id: "d_1".into(),
                state: State::Running.as_str(),
                peers: 3,
                uptime_s: 8812,
                restarts: 0,
                tick_p95_ms: Some(4.1),
                max_players: Some(8),
                mem_mb: Some(203),
                mem_peak_mb: Some(311),
                last_lines: vec!["listening on 30017".into()],
                lobby_code: Some("UQK7RM".into()),
                port: None,
                relay: Some("us-east.relay.fopull.com:7788".into()),
                game_key_prefix: Some("fk_live_432N".into()),
            }],
        };
        let v = r.to_json();
        assert_eq!(v["box"]["host"], "us-east-1");
        assert_eq!(v["box"]["version"], "0.89.1", "the box names its own version (0232)");
        assert_eq!(v["deployments"][0]["game_key_prefix"], "fk_live_432N");
        assert_eq!(v["deployments"][0]["state"], "running");
        assert_eq!(v["deployments"][0]["peers"], 3);
        assert_eq!(v["deployments"][0]["lobby_code"], "UQK7RM");
        assert_eq!(v["deployments"][0]["relay"], "us-east.relay.fopull.com:7788");
        // The numbers a slot is about to be priced from (`floptle/0214`).
        assert_eq!(v["deployments"][0]["mem_mb"], 203);
        assert_eq!(v["deployments"][0]["mem_peak_mb"], 311);
        assert_eq!(v["deployments"][0]["max_players"], 8);
    }

    /// ⚠ **The engine's own ceiling reaches the wire** (`floptle/0221`).
    ///
    /// The server has written `max_players` into its status file all along; the
    /// agent parsed that file and carried every field but this one, so the
    /// control plane stored null about a number the box knew. That is the same
    /// seam as `port` and `relay` before it — **third time in one direction** —
    /// so this asserts the whole trip: file text in, wire JSON out.
    #[test]
    fn the_engines_own_player_ceiling_survives_the_trip_from_the_status_file() {
        let file = r#"{"peers":0,"max_players":8,"uptime_s":15,"lobby_code":"U3Z458"}"#;
        let s: ServerStatus = serde_json::from_str(file).expect("the live status file");
        assert_eq!(s.max_players, Some(8), "the file says 8 and the parse lost it");

        let r = Report {
            box_: BoxStats::default(),
            deployments: vec![DeploymentStatus {
                deployment_id: "d_1".into(),
                state: State::Running.as_str(),
                peers: s.peers,
                uptime_s: s.uptime_s,
                restarts: 0,
                tick_p95_ms: None,
                max_players: s.max_players,
                mem_mb: None,
                mem_peak_mb: None,
                last_lines: vec![],
                lobby_code: s.lobby_code.clone(),
                port: None,
                relay: None,
                game_key_prefix: None,
            }],
        };
        assert_eq!(r.to_json()["deployments"][0]["max_players"], 8);
    }

    /// ⚠ **Every field the server writes reaches the wire, or is named here as
    /// deliberately left behind** (`floptle/0229`, the fourth time). The file
    /// below is what `dedicated.rs::status_document` writes on `us-east-1`;
    /// the test runs it through the SAME function the agent's report does, so
    /// a field the agent parses and then forgets to copy fails here — which
    /// the hand-built `DeploymentStatus` literals above cannot catch, because
    /// they copy by hand too.
    #[test]
    fn every_status_file_field_reaches_the_wire() {
        // Values chosen so none is a default.
        let file = r#"{
            "peers": 3, "max_players": 8, "uptime_s": 8812, "ticks": 528720, "tick_hz": 60,
            "scene": "scenes/lobby.ron", "project": "assets",
            "game_key_prefix": "fk_live_432N", "lobby_code": "UCELXG",
            "port": 30017, "relay": "us-east.relay.fopull.com:7788", "tick_p95_ms": 4.1
        }"#;
        let keys: Vec<String> = serde_json::from_str::<serde_json::Value>(file)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert!(keys.len() >= 12, "the fixture stopped looking like the file: {keys:?}");
        // What the wire does not carry, and why — a field lands here only with
        // a reason the control plane would agree with.
        let left_behind = [
            ("ticks", "a counter with no reader; uptime_s and tick_hz say the same"),
            ("tick_hz", "the deployment's own args set it"),
            ("scene", "the deployment's own args set it"),
            ("project", "always the bundle's `assets`"),
        ];
        let s: ServerStatus = serde_json::from_str(file).expect("the live status file");
        let v = Report {
            box_: BoxStats::default(),
            deployments: vec![DeploymentStatus::from_server(
                "d_1".into(),
                State::Running,
                s,
                0,
                (None, None),
                vec![],
            )],
        }
        .to_json();
        let d = v["deployments"][0].as_object().unwrap();
        let missing: Vec<&String> = keys
            .iter()
            .filter(|k| !left_behind.iter().any(|(f, _)| f == k))
            .filter(|k| !d.contains_key(k.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "the server writes {missing:?} and the agent does not forward it — the fifth time \
             in one direction. Add it to ServerStatus AND DeploymentStatus::from_server, or to \
             left_behind with a reason: {v}"
        );
        assert_eq!(d["game_key_prefix"], "fk_live_432N");
        assert_eq!(d["max_players"], 8);
        assert_eq!(d["lobby_code"], "UCELXG");
        assert_eq!(d["port"], 30017);
        assert_eq!(d["relay"], "us-east.relay.fopull.com:7788");
        assert!((d["tick_p95_ms"].as_f64().unwrap() - 4.1).abs() < 1e-3, "{v}"); // f32 on the wire
    }

    /// ⚠ **A server that reports no ceiling sends NO field, not `0`.**
    ///
    /// `max_players: 0` is a server that admits nobody. "No cap set" and "a cap
    /// of none" would then be the same JSON, and the control plane would read a
    /// perfectly open server as one refusing every player.
    #[test]
    fn a_server_with_no_ceiling_omits_the_field_rather_than_capping_at_zero() {
        let s: ServerStatus = serde_json::from_str(r#"{"peers":3}"#).expect("parses");
        assert_eq!(s.max_players, None, "absent must not become 0");
        let r = Report {
            box_: BoxStats::default(),
            deployments: vec![DeploymentStatus {
                deployment_id: "d_1".into(),
                state: State::Running.as_str(),
                peers: 3,
                uptime_s: 1,
                restarts: 0,
                tick_p95_ms: None,
                max_players: None,
                mem_mb: None,
                mem_peak_mb: None,
                last_lines: vec![],
                lobby_code: None,
                port: None,
                relay: None,
                game_key_prefix: None,
            }],
        };
        let v = r.to_json();
        assert!(
            !v["deployments"][0].as_object().unwrap().contains_key("max_players"),
            "a cap of zero would read as a server that admits nobody: {v}"
        );
    }

    /// ⚠ **A measurement that failed is ABSENT, never `0`** (`floptle/0213`).
    ///
    /// The control plane reads `mem_free_mb` now and treats a box with little
    /// free memory as full — so a healthy machine whose `/proc/meminfo` this
    /// agent could not read, sending `0`, declares itself out of memory. The
    /// region then looks saturated, and a saturated region is one the control
    /// plane prices a new machine for. This asserts the keys are gone rather
    /// than zero, because a `0` here spends money.
    #[test]
    fn a_measurement_the_box_could_not_take_is_omitted_rather_than_zero() {
        let r = Report {
            box_: BoxStats { host: "us-east-1".into(), ..Default::default() },
            deployments: vec![],
        };
        let v = r.to_json();
        assert_eq!(v["box"]["host"], "us-east-1", "the box still names itself");
        let b = v["box"].as_object().expect("an object");
        for f in ["load1", "mem_free_mb", "disk_free_mb"] {
            assert!(!b.contains_key(f), "{f} was sent as zero rather than omitted: {v}");
        }
    }

    /// The same rule for a deployment nobody can measure.
    ///
    /// A stopped unit has no cgroup to ask. Reporting that as `0 MB` would say
    /// the server is free, which is the one answer that would make the fleet
    /// look cheaper than it is.
    #[test]
    fn an_unmeasurable_deployment_reports_no_memory_rather_than_no_memory_used() {
        let r = Report {
            box_: BoxStats::default(),
            deployments: vec![DeploymentStatus {
                deployment_id: "d_1".into(),
                state: State::Stopped.as_str(),
                peers: 0,
                uptime_s: 0,
                restarts: 0,
                tick_p95_ms: None,
                max_players: None,
                mem_mb: None,
                mem_peak_mb: None,
                last_lines: vec![],
                lobby_code: None,
                port: None,
                relay: None,
                game_key_prefix: None,
            }],
        };
        let v = r.to_json();
        let d = v["deployments"][0].as_object().expect("an object");
        assert!(!d.contains_key("mem_mb"), "0 MB would read as a free server: {v}");
        assert!(!d.contains_key("mem_peak_mb"), "{v}");
    }
}
