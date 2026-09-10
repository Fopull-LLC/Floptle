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

#[derive(Debug, Default, Serialize)]
pub struct BoxStats {
    pub host: String,
    pub load1: f32,
    pub mem_free_mb: u64,
    pub disk_free_mb: u64,
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
    /// **Where this server is actually reachable** (`floptle/0209`, forwarded
    /// by `floptle/0212`): the UDP port it bound, or `None` when it listens on
    /// nothing because it went out through a relay.
    #[serde(default)]
    pub port: Option<u16>,
    /// The relay it registered with, or `None` when it listens directly.
    #[serde(default)]
    pub relay: Option<String>,
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
                load1: 0.4,
                mem_free_mb: 9000,
                disk_free_mb: 40000,
            },
            deployments: vec![DeploymentStatus {
                deployment_id: "d_1".into(),
                state: State::Running.as_str(),
                peers: 3,
                uptime_s: 8812,
                restarts: 0,
                tick_p95_ms: Some(4.1),
                last_lines: vec!["listening on 30017".into()],
                lobby_code: Some("UQK7RM".into()),
                port: None,
                relay: Some("us-east.relay.fopull.com:7788".into()),
            }],
        };
        let v = r.to_json();
        assert_eq!(v["box"]["host"], "us-east-1");
        assert_eq!(v["deployments"][0]["state"], "running");
        assert_eq!(v["deployments"][0]["peers"], 3);
        assert_eq!(v["deployments"][0]["lobby_code"], "UQK7RM");
        assert_eq!(v["deployments"][0]["relay"], "us-east.relay.fopull.com:7788");
    }
}
