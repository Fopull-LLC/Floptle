//! The command line, and where the box's token comes from.
//!
//! Shaped like `floptle-relay`'s deliberately: the two are the only binaries
//! that run unattended on a Fopull box, they take the same kind of token, and an
//! operator who has set one up should recognise the other. That includes
//! `--help` printing a table and exiting 0 — which is how W tells a current
//! binary from the one already on a box.

use std::path::PathBuf;

/// Everything the agent was told to be.
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    /// The region this box serves. It is in every URL, and the control plane
    /// refuses a token that names a different one.
    pub region: String,
    /// Control-plane base URL.
    pub control: String,
    /// The box token, once resolved.
    pub token: Option<String>,
    /// Where bundles and engines live.
    pub root: PathBuf,
    /// Where unit files are written.
    pub units: PathBuf,
    /// Where each deployment's game key is written, `0600 root`, for the
    /// unit's `EnvironmentFile=` (`floptle/0229`). The unit itself is
    /// world-readable, as units are, and carries no secret.
    pub keys: PathBuf,
    /// The directory under `/run` that holds one runtime directory per
    /// deployment, each created by systemd and owned by that server's own
    /// dynamic user — see [`Args::status_file`].
    pub run: PathBuf,
    /// Seconds between polls.
    pub interval: u64,
    /// The relay a dedicated server hosts through, so players still join by a
    /// six-character code (`floptle/0199` §3 — W built for exactly this).
    pub relay: Option<String>,
    /// Do one cycle and exit. What CI and a first run on a box use.
    pub once: bool,
    /// Say what would happen; touch nothing.
    pub dry_run: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            region: "us-east".into(),
            control: "https://fopull.com".into(),
            token: None,
            root: PathBuf::from("/var/lib/floptle-fleet"),
            units: PathBuf::from("/etc/systemd/system"),
            keys: PathBuf::from("/etc/floptle/keys"),
            run: PathBuf::from("/run/floptle-d"),
            interval: 10,
            relay: None,
            once: false,
            dry_run: false,
        }
    }
}

impl Args {
    pub const HELP: &'static str = "\
floptle-fleet — the fleet agent: turns a region's desired deployments into
running dedicated servers, and reports what they are doing.

USAGE
  floptle-fleet --region <id> --token-file <path> [--control <url>]

FLAGS
  --region <id>         region this box serves. Default us-east. The control
                        plane refuses a token that names another one.
  --control <url>       control-plane base URL. Default https://fopull.com.
  --token <t>           this box's token. Prefer --token-file.
  --token-file <path>   read the token from a file, so it is not in a command
                        line every `ps` on the box can read. Under systemd this
                        is $CREDENTIALS_DIRECTORY/fleet-token and is found
                        without the flag.
  --root <dir>          bundles and engine versions. Default
                        /var/lib/floptle-fleet.
  --units <dir>         where unit files are written. Default
                        /etc/systemd/system.
  --keys <dir>          where each server's game key is written, one 0600
                        root-only file per deployment that its unit reads with
                        EnvironmentFile=. The unit carries no secret. Default
                        /etc/floptle/keys.
  --run <dir>           parent of each server's runtime directory, which
                        systemd creates for that server's own user; the status
                        file is <dir>/<deployment>/status.json. Must be under
                        /run. Default /run/floptle-d.
  --relay <addr>        relay a dedicated server hosts through, so players join
                        by lobby code rather than by address.
  --interval <secs>     seconds between polls. Default 10.
  --once                do one cycle and exit. What a first run on a box uses.
  --dry-run             say what would happen and touch nothing.
  --help, -h            this table.

The token is looked for in three places, in order: --token-file, then
$CREDENTIALS_DIRECTORY/fleet-token (systemd LoadCredential), then
FLOPTLE_FLEET_TOKEN. Without one the agent refuses to start rather than polling
an endpoint that will refuse it every ten seconds forever.
";

    /// Parse an argv, minus the program name.
    ///
    /// Hand-rolled rather than clap for the same reason the relay is: this
    /// binary ships to a box on its own and has eleven flags, and a parser
    /// whose whole behaviour is one readable match is easier to be sure of than
    /// a dependency's.
    pub fn parse_argv(argv: &[String]) -> Result<Option<Self>, String> {
        let mut a = Args::default();
        let mut token_file: Option<PathBuf> = None;
        let mut i = 0;
        while i < argv.len() {
            let arg = argv[i].as_str();
            let val = || -> Result<String, String> {
                argv.get(i + 1).cloned().ok_or_else(|| format!("{arg} needs a value"))
            };
            match arg {
                "--help" | "-h" => return Ok(None),
                "--region" => {
                    a.region = val()?;
                    i += 1;
                }
                "--control" => {
                    a.control = val()?.trim_end_matches('/').to_string();
                    i += 1;
                }
                "--token" => {
                    a.token = Some(val()?);
                    i += 1;
                }
                "--token-file" => {
                    token_file = Some(PathBuf::from(val()?));
                    i += 1;
                }
                "--root" => {
                    a.root = PathBuf::from(val()?);
                    i += 1;
                }
                "--units" => {
                    a.units = PathBuf::from(val()?);
                    i += 1;
                }
                "--keys" => {
                    a.keys = PathBuf::from(val()?);
                    i += 1;
                }
                "--run" => {
                    a.run = PathBuf::from(val()?);
                    i += 1;
                }
                "--relay" => {
                    a.relay = Some(val()?);
                    i += 1;
                }
                "--interval" => {
                    a.interval = val()?.parse().map_err(|_| "--interval takes seconds".to_string())?;
                    i += 1;
                }
                "--once" => a.once = true,
                "--dry-run" => a.dry_run = true,
                other => return Err(format!("unknown flag {other} — try --help")),
            }
            i += 1;
        }
        if a.token.is_none() {
            a.token = resolve_token(token_file.as_deref());
        }
        Ok(Some(a))
    }

    /// Where deployment `id`'s `--status-file` goes:
    /// `<run>/<id>/status.json`.
    ///
    /// **In a directory of its own, not a file beside the others** — the
    /// directory is what a unit can be given. The server runs `DynamicUser=yes`
    /// under `ProtectSystem=strict`, so it can write nowhere it is not handed
    /// explicitly, and the agent's own runtime directory (root, `0755`) was
    /// exactly such a place: `floptle/0200`'s first defect was every deployment
    /// reporting zero players and no lobby code forever, because the file was
    /// never written and nothing said so. Each server now declares
    /// [`Args::runtime_directory`] and systemd creates it owned by that
    /// server's user.
    pub fn status_file(&self, deployment_id: &str) -> PathBuf {
        self.run.join(crate::unit::sanitize(deployment_id)).join("status.json")
    }

    /// The `EnvironmentFile=` holding one deployment's game key.
    pub fn key_file(&self, deployment_id: &str) -> PathBuf {
        self.keys.join(format!("{}.env", crate::unit::sanitize(deployment_id)))
    }

    /// The `RuntimeDirectory=` a deployment's unit declares, relative to
    /// `/run` as systemd wants it: `floptle-d/d_1`.
    ///
    /// `None` when `--run` is not under `/run` — systemd cannot create a
    /// runtime directory anywhere else, and the agent says so rather than
    /// writing a unit whose status file silently never appears.
    ///
    /// **Deliberately not nested under the agent's own `RuntimeDirectory`.**
    /// systemd removes a unit's runtime directory when that unit stops, so a
    /// status directory under the agent's would be deleted out from under every
    /// running server the moment the agent was restarted or upgraded — and the
    /// zeros would come back with nothing in the journal to say why.
    pub fn runtime_directory(&self, deployment_id: &str) -> Option<String> {
        let rel = self.run.strip_prefix("/run").ok()?;
        let rel = rel.to_string_lossy();
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return None;
        }
        Some(format!("{rel}/{}", crate::unit::sanitize(deployment_id)))
    }

    /// `https://fopull.com/api/floptle/v1/cloud/fleet/us-east/desired`
    pub fn desired_url(&self) -> String {
        format!("{}/api/floptle/v1/cloud/fleet/{}/desired", self.control, self.region)
    }

    pub fn status_url(&self) -> String {
        format!("{}/api/floptle/v1/cloud/fleet/{}/status", self.control, self.region)
    }
}

/// Find the box token: an explicit file, then systemd's credential directory,
/// then the environment.
///
/// **`LoadCredential` is the one that matters on the box.** `DynamicUser=yes`
/// cannot read a root-owned `0600` file, and the fix people reach for is
/// `chmod 644`, which hands the region's secret to every user on the machine.
/// systemd's credential directory exists precisely so the unit can be
/// unprivileged and the secret can stay unreadable to everyone else — so the
/// agent looks there without being told to, and the unit that ships with it
/// uses it.
pub fn resolve_token(explicit: Option<&std::path::Path>) -> Option<String> {
    let read = |p: &std::path::Path| -> Option<String> {
        let t = std::fs::read_to_string(p).ok()?;
        let t = t.trim().to_string();
        (!t.is_empty()).then_some(t)
    };
    if let Some(p) = explicit {
        return read(p);
    }
    if let Ok(dir) = std::env::var("CREDENTIALS_DIRECTORY")
        && let Some(t) = read(&std::path::Path::new(&dir).join("fleet-token"))
    {
        return Some(t);
    }
    std::env::var("FLOPTLE_FLEET_TOKEN").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every flag the parser takes is in the help table.**
    ///
    /// The relay's version of this guard was wrong on its first writing and the
    /// lesson transferred: a plain `HELP.contains(flag)` passes on `--token`
    /// because `--token-file` is mentioned in its row, and every short flag is
    /// a prefix of a longer one. So this matches the FLAG COLUMN — the text
    /// before the two-space gutter — and nothing else.
    #[test]
    fn every_flag_the_parser_takes_is_in_the_help_table() {
        let src = include_str!("args.rs");
        // The flags the match arms actually accept.
        let mut taken: Vec<String> = Vec::new();
        for line in src.lines() {
            let t = line.trim();
            if !t.ends_with("=> {") && !t.contains("=> a.") && !t.contains("=> return") {
                continue;
            }
            for piece in t.split("=>").next().unwrap_or("").split('|') {
                let p = piece.trim().trim_matches('"');
                if p.starts_with('-') {
                    taken.push(p.to_string());
                }
            }
        }
        assert!(taken.len() > 8, "only found {taken:?} — the scraper has stopped reading");

        // The flag column of the table: the first token of each indented row.
        let listed: std::collections::HashSet<&str> = Args::HELP
            .lines()
            .filter_map(|l| {
                let t = l.strip_prefix("  ")?;
                if t.starts_with(' ') {
                    return None;
                }
                Some(t.split("  ").next()?.trim())
            })
            .flat_map(|col| col.split(',').map(str::trim))
            .map(|c| c.split(' ').next().unwrap_or(c))
            .collect();

        let missing: Vec<&String> = taken.iter().filter(|f| !listed.contains(f.as_str())).collect();
        assert!(
            missing.is_empty(),
            "flags the parser takes and the table does not document: {missing:?}\nlisted: {listed:?}"
        );
        assert!(Args::HELP.contains("--help"), "the table names itself");
    }

    /// `--help` is a help request, not an unknown flag.
    ///
    /// This is the exact difference W uses to tell a current binary from the
    /// one already on a box: `floptle-server --help` on the July build was read
    /// as a PORT. A fleet agent that did the same would be indistinguishable
    /// from a stale one.
    #[test]
    fn help_is_a_help_request() {
        assert_eq!(Args::parse_argv(&["--help".into()]), Ok(None));
        assert_eq!(Args::parse_argv(&["-h".into()]), Ok(None));
        // …and an unknown flag is an error naming it, not a silent default.
        let e = Args::parse_argv(&["--regoin".into(), "us-east".into()]).unwrap_err();
        assert!(e.contains("--regoin"), "{e}");
    }

    #[test]
    fn the_urls_are_the_endpoints_w_shipped() {
        let a = Args::parse_argv(&[
            "--region".into(),
            "us-east".into(),
            "--control".into(),
            "https://fopull.com/".into(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(a.desired_url(), "https://fopull.com/api/floptle/v1/cloud/fleet/us-east/desired");
        assert_eq!(a.status_url(), "https://fopull.com/api/floptle/v1/cloud/fleet/us-east/status");
        // A trailing slash on --control must not double up.
        assert!(!a.desired_url().contains("//api"));
    }

    #[test]
    fn flags_parse_into_what_they_say() {
        let a = Args::parse_argv(&[
            "--root".into(), "/tmp/r".into(),
            "--units".into(), "/tmp/u".into(),
            "--run".into(), "/tmp/run".into(),
            "--relay".into(), "relay.fopull.com:7788".into(),
            "--interval".into(), "30".into(),
            "--once".into(),
            "--dry-run".into(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(a.root, PathBuf::from("/tmp/r"));
        assert_eq!(a.units, PathBuf::from("/tmp/u"));
        assert_eq!(a.run, PathBuf::from("/tmp/run"));
        assert_eq!(a.relay.as_deref(), Some("relay.fopull.com:7788"));
        assert_eq!(a.interval, 30);
        assert!(a.once && a.dry_run);
    }

    /// An explicit token file is read, and a blank one is not a token.
    /// **The status file is in a directory systemd can hand to the server.**
    ///
    /// The unit's `RuntimeDirectory=` is relative to `/run`, so a `--run`
    /// anywhere else is a unit whose status file silently never appears —
    /// the exact shape of `floptle/0200`'s first defect — and the agent says
    /// so instead of writing one. The id is sanitised on the way in, the same
    /// way the unit name is, because it becomes a path.
    #[test]
    fn the_status_directory_is_the_units_own_and_under_run() {
        let a = Args::default();
        assert_eq!(a.runtime_directory("d_1").as_deref(), Some("floptle-d/d_1"));
        assert_eq!(a.status_file("d_1"), PathBuf::from("/run/floptle-d/d_1/status.json"));
        assert_eq!(
            a.runtime_directory("../etc").as_deref(),
            Some("floptle-d/___etc"),
            "a deployment id is filtered before it becomes a path"
        );
        let elsewhere = Args { run: PathBuf::from("/var/tmp/fleet"), ..Args::default() };
        assert_eq!(elsewhere.runtime_directory("d_1"), None, "systemd cannot create it there");
        let bare = Args { run: PathBuf::from("/run"), ..Args::default() };
        assert_eq!(bare.runtime_directory("d_1"), None, "/run itself is not a directory to own");
    }

    #[test]
    fn a_blank_token_file_is_no_token() {
        let dir = std::env::temp_dir().join(format!("fleet-tok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("t");
        std::fs::write(&good, "  box_tok_abc\n").unwrap();
        assert_eq!(resolve_token(Some(&good)).as_deref(), Some("box_tok_abc"), "trimmed");
        let blank = dir.join("blank");
        std::fs::write(&blank, "\n  \n").unwrap();
        assert_eq!(resolve_token(Some(&blank)), None, "whitespace is not a credential");
        assert_eq!(resolve_token(Some(&dir.join("nope"))), None);
    }
}
