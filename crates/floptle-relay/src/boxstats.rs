//! **What this relay says about its own machine** (`floptle/0215`).
//!
//! The fleet agent gained saturation detection when Floptle Cloud started
//! sizing its own fleet; the relay reported nothing about itself at all, so the
//! way we would have learned it was overloaded is complaints. It is the box a
//! signup surge reaches FIRST — every free-tier player in a region goes through
//! it, long before anyone rents a dedicated server.
//!
//! The four fields the fleet agent sends are sent here under the same names and
//! the same rule, so the control plane consumes both with one code path:
//! ⚠ **a measurement that could not be taken is OMITTED, never sent as zero.**
//! "No free memory" and "did not measure" are opposite facts, and only one of
//! them should stop anything.
//!
//! # Why memory is the wrong headline number for a relay
//!
//! W picked `mem_free_mb` for fleet boxes because memory is what kills a game
//! server. A relay does not fail that way. It holds a code, a key and a peer
//! list per lobby — kilobytes — and forwards datagrams without accumulating
//! them. On a `VM.Standard.E2.1.Micro` (1 OCPU, 1 GB, 0.48 Gbps) the ceiling it
//! actually reaches is **the link**, and the fields that see it coming are, in
//! order:
//!
//! 1. `egress_bps` against 0.48 Gbps. A relay multiplies traffic — one datagram
//!    into an eight-player lobby leaves seven times — so egress saturates while
//!    ingress still looks quiet, and it is egress that is metered and capped.
//! 2. `rx_drops`, which is not a proxy for anything: it is datagrams the kernel
//!    has **already thrown away** because this process did not read them in
//!    time. Non-zero and rising is not a warning that saturation is coming, it
//!    is players losing packets right now.
//! 3. `step_p95_ms`, the forwarding loop's own period. It stretches before
//!    drops begin, which makes it the earliest honest signal here.
//! 4. `load1` — but only read against `cores`. This box has **one** OCPU, so a
//!    `load1` of 0.9 is nearly saturated where the same figure on an eight-core
//!    fleet box is an idle machine. `cores` is sent so the two cannot be
//!    compared as though they meant the same thing.
//!
//! `mem_free_mb` is sent anyway, for uniformity and because it costs nothing —
//! just do not rank on it here.

/// This machine and this relay, as the usage POST reports them.
///
/// Every measurement is optional and an absent one is left off the wire; see
/// the module docs for why that matters more than it looks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RelayBox {
    /// What the box calls itself. Always sent, even when it had to be guessed,
    /// because it is what identifies the row.
    pub host: String,
    pub load1: Option<f32>,
    /// ⚠ **How many CPUs `load1` is out of.** Without it a load figure from a
    /// 1-OCPU relay reads like one from a fleet box and means the opposite.
    pub cores: Option<u32>,
    pub mem_free_mb: Option<u64>,
    pub disk_free_mb: Option<u64>,
    /// Payload bytes per second **sent on** over the interval just reported —
    /// the half that is metered, capped, and reached first.
    pub egress_bps: Option<u64>,
    /// Payload bytes per second received over the same interval.
    pub ingress_bps: Option<u64>,
    /// Lobbies open right now, and players in them.
    pub lobbies: u32,
    pub peers: u32,
    /// Datagrams the kernel discarded on this process's own UDP sockets because
    /// they were not read in time. **Cumulative since the socket opened**, so
    /// what matters is that it moves, not what it reads.
    pub rx_drops: Option<u64>,
    /// Bytes sitting unread in those sockets' receive queues at the moment of
    /// the report — the backlog that becomes `rx_drops` if it keeps growing.
    pub rx_queue_bytes: Option<u64>,
    /// The 95th-percentile period of the forwarding loop over the interval.
    /// The reference binary aims for about a millisecond.
    pub step_p95_ms: Option<f32>,
    /// Messages the relay's own limits refused or dropped — a connection over
    /// its budget, an address opening lobbies too fast, a frame too large for
    /// its leg (`floptle_net::relay::RelayLimits`). **Cumulative** like
    /// `rx_drops`, and like it what matters is that it moves: a relay being
    /// leaned on shows here before it shows anywhere else.
    pub limit_drops: Option<u64>,
}

/// Read everything this module can measure about the machine.
///
/// The relay's own counters (`egress_bps`, `lobbies`, `step_p95_ms`, …) are
/// filled in by the policy, which is the only thing that knows them.
pub fn host_metrics() -> RelayBox {
    RelayBox {
        host: std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into()),
        load1: read_first_field("/proc/loadavg"),
        cores: std::thread::available_parallelism().ok().map(|n| n.get() as u32),
        mem_free_mb: mem_available_mb(),
        disk_free_mb: disk_free_mb("/"),
        ..Default::default()
    }
}

fn read_first_field(path: &str) -> Option<f32> {
    std::fs::read_to_string(path).ok()?.split_whitespace().next()?.parse().ok()
}

/// `MemAvailable` in megabytes — what is actually reusable, rather than
/// `MemFree`, which on a box with a page cache reads alarmingly low while
/// nothing is wrong.
fn mem_available_mb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = s
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kb / 1024)
}

/// Free megabytes on the filesystem holding `path`, or `None` — never zero.
fn disk_free_mb(path: &str) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let c = CString::new(path).ok()?;
        // SAFETY: `statvfs` fills a POD struct; the path is a valid C string
        // and the struct is zeroed before the call.
        unsafe {
            let mut s: Statvfs = std::mem::zeroed();
            if statvfs(c.as_ptr(), &mut s) == 0 {
                return Some(s.f_bavail.saturating_mul(s.f_frsize) / (1024 * 1024));
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// **Receive-queue backlog and drops on this process's own UDP sockets**, as
/// `(rx_queue_bytes, drops)`.
///
/// Matched by socket **inode** rather than by port. The relay knows its port,
/// but threading it down here to build a hex string and compare it against
/// `/proc/net/udp`'s `local_address` column would be a second place that has to
/// agree about what "this relay's socket" means — and it would silently match
/// nothing the day the relay binds a second one or listens on v6. The inodes
/// under `/proc/self/fd` are what this process actually holds, by definition.
pub fn socket_pressure() -> (Option<u64>, Option<u64>) {
    let mine = own_socket_inodes();
    if mine.is_empty() {
        return (None, None);
    }
    let mut queue = 0u64;
    let mut drops = 0u64;
    let mut seen = false;
    for table in ["/proc/net/udp", "/proc/net/udp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            // sl local rem st tx:rx tr:when retrnsmt uid timeout inode … drops
            if f.len() < 10 {
                continue;
            }
            if !f[9].parse::<u64>().is_ok_and(|i| mine.contains(&i)) {
                continue;
            }
            seen = true;
            if let Some(rx) = f[4].split(':').nth(1).and_then(|h| u64::from_str_radix(h, 16).ok()) {
                queue += rx;
            }
            if let Some(d) = f.last().and_then(|d| d.parse::<u64>().ok()) {
                drops += d;
            }
        }
    }
    if seen { (Some(queue), Some(drops)) } else { (None, None) }
}

/// The inode of every socket this process holds open.
fn own_socket_inodes() -> std::collections::HashSet<u64> {
    let mut out = std::collections::HashSet::new();
    let Ok(dir) = std::fs::read_dir("/proc/self/fd") else { return out };
    for e in dir.flatten() {
        if let Ok(target) = std::fs::read_link(e.path())
            && let Some(i) = target
                .to_str()
                .and_then(|s| s.strip_prefix("socket:["))
                .and_then(|s| s.strip_suffix(']'))
                .and_then(|s| s.parse::<u64>().ok())
        {
            out.insert(i);
        }
    }
    out
}

// A minimal `statvfs` binding rather than a `libc` dependency for one call,
// matching the fleet agent's.
#[cfg(unix)]
#[repr(C)]
#[allow(non_snake_case)]
struct Statvfs {
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
    fn statvfs(path: *const std::ffi::c_char, buf: *mut Statvfs) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The box names itself even when it can measure nothing else.**
    ///
    /// The host field is what identifies the row; a report that cannot say
    /// which machine it came from is not worth sending.
    #[test]
    fn the_relay_always_names_its_host() {
        let b = host_metrics();
        assert!(!b.host.is_empty(), "a report has to say which box it is");
    }

    /// This box is Linux and these files exist, so a `None` here is a bug in
    /// the parsing rather than a machine that declined to answer.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_host_numbers_actually_parse_on_a_real_machine() {
        let b = host_metrics();
        assert!(b.load1.is_some(), "/proc/loadavg did not parse");
        assert!(b.mem_free_mb.is_some_and(|m| m > 0), "/proc/meminfo did not parse");
        assert!(b.disk_free_mb.is_some(), "statvfs failed");
        // ⚠ The one that makes `load1` mean anything. A relay is 1 OCPU.
        assert!(b.cores.is_some_and(|c| c > 0), "load1 is unreadable without cores");
    }

    /// ⚠ **A socket table that says nothing gives `None`, not `0`.**
    ///
    /// Zero drops is the good news a relay reports when it is keeping up. A
    /// relay that could not read `/proc/net/udp` reporting the same zero is the
    /// failure looking exactly like health — which is the shape this codebase
    /// keeps paying for.
    #[test]
    fn an_unreadable_socket_table_is_not_zero_drops() {
        let (q, d) = socket_pressure();
        assert_eq!(q.is_some(), d.is_some(), "backlog and drops answer together");
    }

    /// **The socket really is found in `/proc/net/udp`.**
    ///
    /// The consistency check above passes just as happily when the inode match
    /// never fires and the answer is always `None` — which is the whole feature
    /// silently reporting nothing. This holds an actual UDP socket open and
    /// requires the lookup to see it, so a `/proc` format change or a broken
    /// inode parse fails here rather than shipping as a permanently quiet
    /// metric.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_socket_this_process_holds_is_found_in_the_kernels_table() {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("a local UDP socket");
        let (q, d) = socket_pressure();
        assert!(q.is_some() && d.is_some(), "our own open socket was not found in /proc/net/udp");
        // Nothing has been sent to it, so it is idle rather than backed up —
        // this is the reading that proves a healthy relay reports real zeros
        // and not the absent ones the guard above would also accept.
        assert_eq!(d, Some(0), "a socket nobody has written to has dropped nothing");
        drop(sock);
    }
}
