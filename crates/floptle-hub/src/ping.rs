//! The anonymous daily install ping: the Hub tells fopull.com once a day that
//! an install exists, so downloads can be told apart from installs.
//!
//! The contract is `contracts/install-ping.md` in the platform repo. What it
//! asks of a client, and where this file does it:
//!
//! * a random UUID v4 made once, kept in the Hub's own config directory and
//!   never derived from the machine or the user ([`State`], [`uuid_v4`]);
//! * at most one ping a UTC day, in the background, ten seconds at most, two
//!   retries after a network error or a 5xx and none after a 400 or 429
//!   ([`start`], [`answer_of`]);
//! * a notice before the first ping, a setting that turns it off and deletes
//!   the id, and `FOPULL_NO_PING` overriding both ([`env_says_off`], [`forget`]).
//!
//! The editor never pings: the Hub is the install.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Paths;

pub const ENDPOINT: &str = "https://fopull.com/api/v1/installs/ping";
/// What the ping holds and how to turn it off, in plain words.
pub const PRIVACY_URL: &str = "https://fopull.com/privacy#install-ping";
pub const PRODUCT: &str = "floptle-hub";

/// What the notice and the setting say the ping is.
pub const NOTICE: &str = "The Hub sends an anonymous ping once a day: a random install id, the Hub's version, \
your OS and CPU type, and its update channel. Nothing that identifies you. Turn it off in Settings.";

/// Seconds to wait before each retry after a network error or a 5xx.
const RETRY_AFTER: [u64; 2] = [60, 600];

/// The install id and the UTC day of the last ping the server recorded.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct State {
    pub install_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent: Option<String>,
}

fn state_path(config_dir: &Path) -> PathBuf {
    config_dir.join("install-ping.json")
}

/// Is the ping turned off for this process? Any non-empty `FOPULL_NO_PING`
/// other than `0` turns it off, whatever the setting says.
pub fn env_says_off(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// A UUID v4 from 16 random bytes: the version and variant bits set, the
/// rest as given.
pub fn uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// The UTC calendar day `secs` seconds after the Unix epoch, as `YYYY-MM-DD`.
pub fn utc_day(secs: u64) -> String {
    // Howard Hinnant's days-to-civil.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn today() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    utc_day(secs)
}

/// The OS as the contract names it.
pub fn os_name(os: &str) -> &'static str {
    match os {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        _ => "other",
    }
}

/// The request body for one ping.
pub fn body(install_id: &str, channel: &str) -> serde_json::Value {
    serde_json::json!({
        "install_id": install_id,
        "product": PRODUCT,
        "version": env!("CARGO_PKG_VERSION"),
        "os": os_name(std::env::consts::OS),
        "arch": std::env::consts::ARCH,
        "channel": channel,
    })
}

/// What the server's answer means for this client.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// 204: recorded. Nothing more today.
    Recorded,
    /// 400 or 429: not today, and no retry.
    Refused,
    /// A 5xx or no answer: worth a retry later in this run.
    Later,
}

/// [`Answer`] from an HTTP status, `None` being no answer at all.
pub fn answer_of(status: Option<u16>) -> Answer {
    match status {
        Some(204) | Some(200) => Answer::Recorded,
        Some(s) if (500..600).contains(&s) => Answer::Later,
        None => Answer::Later,
        Some(_) => Answer::Refused,
    }
}

/// Load the state, making an install id if there is none. `None` when no id
/// could be made or saved, in which case nothing is sent.
fn load_or_create(config_dir: &Path) -> Option<State> {
    let path = state_path(config_dir);
    if let Some(s) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<State>(&t).ok())
        && !s.install_id.is_empty()
    {
        return Some(s);
    }
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).ok()?;
    let state = State { install_id: uuid_v4(raw), last_sent: None };
    save(config_dir, &state).ok()?;
    Some(state)
}

fn save(config_dir: &Path, state: &State) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let text = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(state_path(config_dir), text)
}

/// Turn the ping off for good: the install id goes, so nothing on disk links
/// this machine to anything it ever sent.
pub fn forget(paths: &Paths) {
    let _ = std::fs::remove_file(state_path(&paths.config));
}

/// Should this run ping at all? Turned on, the notice shown, not opted out by
/// the environment, and not already recorded today.
pub fn wanted(enabled: bool, notice_seen: bool, env_off: bool, last_sent: Option<&str>, today: &str) -> bool {
    enabled && notice_seen && !env_off && last_sent != Some(today)
}

/// Send today's ping on a background thread, if this run should. Nothing
/// waits on it and it never reports an error to the user.
pub fn start(paths: &Paths, enabled: bool, notice_seen: bool, channel: String) {
    let env_off = env_says_off(std::env::var("FOPULL_NO_PING").ok().as_deref());
    if !enabled || !notice_seen || env_off {
        return;
    }
    let config_dir = paths.config.clone();
    std::thread::spawn(move || {
        let Some(mut state) = load_or_create(&config_dir) else { return };
        let day = today();
        if !wanted(true, true, false, state.last_sent.as_deref(), &day) {
            return;
        }
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
        let payload = body(&state.install_id, &channel);
        for wait in std::iter::once(0).chain(RETRY_AFTER) {
            std::thread::sleep(Duration::from_secs(wait));
            let status = match agent.post(ENDPOINT).send_json(payload.clone()) {
                Ok(r) => Some(r.status()),
                Err(ureq::Error::Status(code, _)) => Some(code),
                Err(_) => None,
            };
            match answer_of(status) {
                Answer::Recorded => {
                    state.last_sent = Some(day);
                    let _ = save(&config_dir, &state);
                    return;
                }
                Answer::Refused => {
                    log::info!("install ping refused ({status:?}); not again today");
                    return;
                }
                Answer::Later => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v4 UUID has its version nibble 4 and its variant bits 10, and the
    /// rest is the random bytes, in the 8-4-4-4-12 shape the server checks.
    #[test]
    fn an_install_id_is_a_v4_uuid() {
        let id = uuid_v4([0xff; 16]);
        assert_eq!(id, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        let id = uuid_v4([0; 16]);
        assert_eq!(id, "00000000-0000-4000-8000-000000000000");
        assert_eq!(id.len(), 36);
    }

    /// Days are UTC calendar days, across a leap day and a year's end.
    #[test]
    fn a_day_is_the_utc_calendar_day() {
        assert_eq!(utc_day(0), "1970-01-01");
        assert_eq!(utc_day(951_782_400), "2000-02-29");
        assert_eq!(utc_day(1_798_761_599), "2026-12-31");
        assert_eq!(utc_day(1_798_761_600), "2027-01-01");
    }

    /// `FOPULL_NO_PING` turns it off for any non-empty value but `0`.
    #[test]
    fn the_environment_can_turn_it_off() {
        assert!(!env_says_off(None));
        assert!(!env_says_off(Some("")));
        assert!(!env_says_off(Some("0")));
        assert!(env_says_off(Some("1")));
        assert!(env_says_off(Some("yes")));
    }

    /// **Once a day, only when allowed.** Off, before the notice, opted out by
    /// the environment, or already recorded today: no request at all.
    #[test]
    fn it_pings_at_most_once_a_day_and_only_when_allowed() {
        assert!(wanted(true, true, false, None, "2026-10-19"));
        assert!(wanted(true, true, false, Some("2026-10-18"), "2026-10-19"));
        assert!(!wanted(true, true, false, Some("2026-10-19"), "2026-10-19"), "twice in a day");
        assert!(!wanted(false, true, false, None, "2026-10-19"), "turned off");
        assert!(!wanted(true, false, false, None, "2026-10-19"), "before the notice");
        assert!(!wanted(true, true, true, None, "2026-10-19"), "FOPULL_NO_PING");
    }

    /// A 204 is done, a 400 or 429 stops for the day, and only a 5xx or no
    /// answer is retried.
    #[test]
    fn the_answer_decides_whether_to_try_again() {
        assert_eq!(answer_of(Some(204)), Answer::Recorded);
        assert_eq!(answer_of(Some(400)), Answer::Refused);
        assert_eq!(answer_of(Some(429)), Answer::Refused);
        assert_eq!(answer_of(Some(503)), Answer::Later);
        assert_eq!(answer_of(None), Answer::Later);
    }

    /// The body carries exactly the contract's fields, and the product the
    /// server knows the Hub by.
    #[test]
    fn the_body_is_the_contracts() {
        let b = body("00000000-0000-4000-8000-000000000000", "stable");
        let o = b.as_object().unwrap();
        let mut keys: Vec<&str> = o.keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["arch", "channel", "install_id", "os", "product", "version"]);
        assert_eq!(o["product"], "floptle-hub");
        assert!(["windows", "macos", "linux", "other"].contains(&o["os"].as_str().unwrap()));
        assert!(serde_json::to_vec(&b).unwrap().len() <= 2048);
    }

    /// The id is made once and kept; forgetting it removes the file, so the
    /// next run is a new install.
    #[test]
    fn the_id_is_kept_until_it_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { data: dir.path().join("data"), config: dir.path().join("config") };
        let a = load_or_create(&paths.config).unwrap();
        let b = load_or_create(&paths.config).unwrap();
        assert_eq!(a.install_id, b.install_id, "a second run made a new id");
        assert!(state_path(&paths.config).exists());
        forget(&paths);
        assert!(!state_path(&paths.config).exists(), "turning it off left the id behind");
        let c = load_or_create(&paths.config).unwrap();
        assert_ne!(a.install_id, c.install_id);
    }
}
