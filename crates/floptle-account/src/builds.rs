//! Uploading a server bundle to Floptle Cloud as the developer (`floptle ship`).
//!
//! Three calls (contracts/cloud-hosting.md §4, served under `/games/{slug}/builds`): reserve a build for the
//! bundle's digest, send the bytes to the signed URL that comes back, and
//! complete. The bytes go in pieces whenever the site offers them
//! (`upload.chunk_bytes`), because Cloudflare refuses any single request over
//! 100 MB, which is how a 109 MB bundle once sat at 0% forever.
//!
//! The site keeps what it has received. So the upload asks where it stands
//! before sending anything, carries on from there, and treats three answers as
//! part of the protocol rather than failures:
//!
//! - **409 `{received}`**: a piece did not start where the site's copy ends.
//!   Carry on from `received`.
//! - **403**: the signed URL expired (it lasts 30 minutes). Reserve the same
//!   digest again, which hands back the same build with a fresh URL and keeps
//!   the bytes already sent.
//! - A dropped connection or a 5xx: wait, ask again where it stands, go on.
//!
//! That is also what makes a killed upload resume: run it again with the same
//! bundle and the first question finds the bytes already there.
//!
//! The developer's token never leaves this crate, like every other token here.
//! The signed URL carries its own authority, so the token is not sent to it.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

/// The bundle to upload: the file and what the reserve call says about it.
pub struct Bundle<'a> {
    /// The registered game's slug.
    pub game: &'a str,
    pub path: &'a Path,
    /// Lower-case hex SHA-256 of the file.
    pub sha256: &'a str,
    pub size: u64,
    /// The engine the manifest pins.
    pub engine_version: &'a str,
    pub label: Option<&'a str>,
}

/// How far an upload has got, for a progress line.
#[derive(Clone, Debug, PartialEq)]
pub enum Progress {
    /// The build exists; `already` bytes of it were on the site before this run.
    Reserved { build_id: String, already: u64 },
    /// `sent` of `total` bytes are on the site.
    Sent { sent: u64, total: u64 },
    /// Every byte is there; asking the site to check the digest.
    Completing,
}

/// Single requests larger than this never reach the site (Cloudflare's own
/// limit), so a site that offers no pieces cannot take a bundle past it.
pub const SINGLE_PUT_LIMIT: u64 = 100 * 1024 * 1024;

/// What the bundles say they are: a bundle carries no binary, so the box's
/// own engine runs it, and this is the family of boxes that do.
pub const PLATFORM: &str = "server-linux-aarch64";

/// How many times one piece is tried before the upload gives up, and how many
/// fresh URLs one run may ask for.
const TRIES: u32 = 6;
const RESERVES: u32 = 4;

/// Waits between tries: short, then longer. Tests pass zeros.
pub(crate) type Backoff = fn(u32) -> Duration;

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(4))
}

/// Lower-case hex SHA-256 of a file, read in pieces: the digest the reserve
/// call names a bundle by.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

struct Reservation {
    build_id: String,
    url: String,
    chunk: Option<u64>,
    ready: bool,
}

/// A game slug is one URL segment and nothing else.
fn check_slug(game: &str) -> Result<(), String> {
    if game.is_empty() || !game.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(format!("'{game}' is not a game slug (letters, digits, - and _)"));
    }
    Ok(())
}

fn error_of(status: u16, body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let said = v
        .get("error_description")
        .or_else(|| v.get("error"))
        .or_else(|| v.get("message"))
        .and_then(|s| s.as_str())
        .map(str::to_string);
    match said {
        Some(s) => format!("fopull.com answered {status}: {s}"),
        None => format!("fopull.com answered {status}"),
    }
}

fn received_of(body: &str) -> Option<(u64, bool)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let received = v.get("received")?.as_u64()?;
    Some((received, v.get("complete").and_then(|c| c.as_bool()).unwrap_or(false)))
}

/// One HTTP exchange, reduced to what the protocol reads: `Err` is a transport
/// failure, `Ok` any status with its body.
fn exchange(req: ureq::Request, body: Option<&[u8]>) -> Result<(u16, String), String> {
    let res = match body {
        Some(b) => req.send_bytes(b),
        None => req.call(),
    };
    match res {
        Ok(r) | Err(ureq::Error::Status(_, r)) => {
            let status = r.status();
            let mut text = String::new();
            let _ = r.into_reader().take(64 * 1024).read_to_string(&mut text);
            Ok((status, text))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The upload, given a token. [`crate::Account::upload_build`] is the public
/// door and fetches the token; tests call this against a fake site.
pub(crate) fn upload_with_token(
    base: &str,
    token: &str,
    b: &Bundle,
    progress: &mut dyn FnMut(Progress) -> bool,
    wait: Backoff,
) -> Result<String, String> {
    check_slug(b.game)?;
    let base = base.trim_end_matches('/');
    // `/games/{slug}/builds` under the API, which is where fopull.com serves
    // it. (cloud-hosting.md §4 writes these as `/cloud/games/…`; every
    // spelling under `/cloud` answers the framework's "route not found".)
    let builds = format!("{base}{}/games/{}/builds", crate::cloud::API_PREFIX, b.game);
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(120)).build();

    let reserve = || -> Result<Reservation, String> {
        let mut body = serde_json::json!({
            "platform": PLATFORM,
            "sha256": b.sha256,
            "size_bytes": b.size,
            "engine_version": b.engine_version,
        });
        if let Some(l) = b.label {
            body["label"] = serde_json::Value::String(l.to_string());
        }
        let req = agent
            .post(&builds)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/json")
            .set("Content-Type", "application/json");
        let (status, text) = exchange(req, Some(body.to_string().as_bytes()))
            .map_err(|e| format!("could not reach fopull.com: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(error_of(status, &text));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("fopull.com's reserve answer is not JSON: {e}"))?;
        let build_id = v.get("build_id").and_then(|s| s.as_str()).ok_or("the reserve answer has no build_id")?;
        let upload = v.get("upload");
        Ok(Reservation {
            build_id: build_id.to_string(),
            url: upload.and_then(|u| u.get("url")).and_then(|s| s.as_str()).unwrap_or_default().to_string(),
            chunk: upload.and_then(|u| u.get("chunk_bytes")).and_then(|c| c.as_u64()).filter(|c| *c > 0),
            ready: v.get("status").and_then(|s| s.as_str()) == Some("ready"),
        })
    };

    let mut file = std::fs::File::open(b.path).map_err(|e| format!("open {}: {e}", b.path.display()))?;
    let mut res = reserve()?;
    if res.ready {
        // This exact bundle was shipped before: nothing to send.
        progress(Progress::Reserved { build_id: res.build_id.clone(), already: b.size });
        return Ok(res.build_id);
    }
    if res.url.is_empty() {
        return Err("fopull.com reserved the build but gave no upload URL".into());
    }

    match res.chunk {
        None => {
            if b.size > SINGLE_PUT_LIMIT {
                return Err(format!(
                    "this bundle is {} MB and fopull.com offered no piece upload; a single request \
                     over 100 MB never reaches it",
                    b.size / (1024 * 1024)
                ));
            }
            progress(Progress::Reserved { build_id: res.build_id.clone(), already: 0 });
            let mut bytes = Vec::with_capacity(b.size as usize);
            file.read_to_end(&mut bytes).map_err(|e| format!("read the bundle: {e}"))?;
            let req = agent.put(&res.url).set("Content-Type", "application/octet-stream");
            let (status, text) = exchange(req, Some(&bytes)).map_err(|e| format!("upload: {e}"))?;
            if !(200..300).contains(&status) {
                return Err(error_of(status, &text));
            }
            progress(Progress::Sent { sent: b.size, total: b.size });
        }
        Some(_) => {
            let total = b.size;
            // Where the site stands. `None` = ask again.
            let probe = |url: &str| -> Result<(u16, String), String> {
                let req = agent.put(url).set("Content-Range", &format!("bytes */{total}"));
                exchange(req, Some(&[]))
            };
            let mut received: Option<u64> = None;
            let mut tries = 0u32;
            let mut reserves = 1u32;
            let mut announced = false;
            let mut buf = Vec::new();
            loop {
                let chunk = res.chunk.unwrap_or(8 * 1024 * 1024);
                let at = match received {
                    Some(r) => r,
                    None => match probe(&res.url) {
                        Ok((s, text)) if (200..300).contains(&s) || s == 409 || s == 308 => {
                            received_of(&text).map(|(r, _)| r).unwrap_or(0)
                        }
                        Ok((403, _)) if reserves < RESERVES => {
                            reserves += 1;
                            res = reserve()?;
                            continue;
                        }
                        Ok((s, text)) if s < 500 => return Err(error_of(s, &text)),
                        Ok(_) | Err(_) => {
                            tries += 1;
                            if tries >= TRIES {
                                return Err("fopull.com stopped answering; run the same command again to carry on".into());
                            }
                            std::thread::sleep(wait(tries));
                            continue;
                        }
                    },
                };
                if !announced {
                    announced = true;
                    progress(Progress::Reserved { build_id: res.build_id.clone(), already: at });
                }
                if at >= total {
                    break;
                }
                let end = (at + chunk).min(total);
                buf.resize((end - at) as usize, 0);
                file.seek(SeekFrom::Start(at)).map_err(|e| format!("read the bundle: {e}"))?;
                file.read_exact(&mut buf).map_err(|e| format!("read the bundle: {e}"))?;
                let req = agent
                    .put(&res.url)
                    .set("Content-Type", "application/octet-stream")
                    .set("Content-Range", &format!("bytes {at}-{}/{total}", end - 1));
                match exchange(req, Some(&buf)) {
                    Ok((s, text)) if (200..300).contains(&s) => {
                        tries = 0;
                        let next = received_of(&text).map(|(r, _)| r).unwrap_or(end);
                        received = Some(next);
                        if !progress(Progress::Sent { sent: next, total }) {
                            return Err("stopped; run the same command again to carry on".into());
                        }
                    }
                    // Not where the site's copy ends: its answer says where.
                    Ok((409, text)) => received = received_of(&text).map(|(r, _)| r),
                    Ok((403, _)) if reserves < RESERVES => {
                        reserves += 1;
                        res = reserve()?;
                        received = None;
                    }
                    Ok((s, text)) if s < 500 => return Err(error_of(s, &text)),
                    Ok(_) | Err(_) => {
                        tries += 1;
                        if tries >= TRIES {
                            return Err("fopull.com stopped answering; run the same command again to carry on".into());
                        }
                        std::thread::sleep(wait(tries));
                        received = None;
                    }
                }
            }
        }
    }

    progress(Progress::Completing);
    let req = agent
        .post(&format!("{builds}/{}/complete", res.build_id))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json");
    let (status, text) = exchange(req, None).map_err(|e| format!("could not reach fopull.com: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(error_of(status, &text));
    }
    Ok(res.build_id)
}

impl crate::Account {
    /// Upload a server bundle as the signed-in developer, in pieces where the
    /// site offers them, and complete the build. **Blocking** for as long as
    /// the upload takes. `progress` returning `false` stops it (the bytes sent
    /// so far stay on the site, and running it again carries on).
    pub fn upload_build(&self, b: &Bundle, progress: &mut dyn FnMut(Progress) -> bool) -> Result<String, String> {
        let token = self.access_token()?;
        upload_with_token(self.base(), &token, b, progress, backoff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    /// A fake fopull.com that speaks the contract's piece protocol.
    #[derive(Default)]
    struct Site {
        chunk: Option<u64>,
        stored: Vec<u8>,
        /// Bytes the site accepted, over every run: a resume that re-sent
        /// anything shows up here as more than the file.
        accepted: u64,
        /// Signed URLs handed out; only the newest works once one expires.
        url_gen: u32,
        expire_after_pieces: Option<u32>,
        pieces: u32,
        /// Accept the next piece, then drop the reply.
        lose_next_reply: bool,
        /// The next probe claims nothing has arrived.
        stale_next_probe: bool,
        completed: bool,
        requests: Vec<String>,
    }

    /// (request line, lower-cased headers, body).
    type Request = (String, Vec<(String, String)>, Vec<u8>);

    fn read_request(c: &mut TcpStream) -> Option<Request> {
        let mut got = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = c.read(&mut buf).ok()?;
            if n == 0 {
                return None;
            }
            got.extend_from_slice(&buf[..n]);
            if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&got[..end]).to_string();
                let mut lines = head.lines();
                let line = lines.next()?.to_string();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())))
                    .collect();
                let len = headers.iter().find(|(k, _)| k == "content-length").map(|(_, v)| v.parse().unwrap()).unwrap_or(0);
                while got.len() < end + 4 + len {
                    let n = c.read(&mut buf).ok()?;
                    got.extend_from_slice(&buf[..n]);
                }
                return Some((line, headers, got[end + 4..end + 4 + len].to_vec()));
            }
        }
    }

    fn reply(c: &mut TcpStream, status: &str, body: &str) {
        let _ = write!(
            c,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    fn serve(site: Arc<Mutex<Site>>, total: u64) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let me = base.clone();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let Some((line, headers, body)) = read_request(&mut c) else { continue };
                let mut s = site.lock().unwrap();
                s.requests.push(line.clone());
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                let range = headers.iter().find(|(k, _)| k == "content-range").map(|(_, v)| v.clone());
                if line.starts_with("POST") && path.ends_with("/builds") {
                    s.url_gen += 1;
                    let chunk = s.chunk.map(|c| format!(",\"chunk_bytes\":{c}")).unwrap_or_default();
                    let body = format!(
                        "{{\"build_id\":\"b_1\",\"upload\":{{\"method\":\"PUT\",\"url\":\"{me}/up/{}\"{chunk}}}}}",
                        s.url_gen
                    );
                    reply(&mut c, "201 Created", &body);
                } else if line.starts_with("POST") && path.ends_with("/complete") {
                    s.completed = true;
                    reply(&mut c, "200 OK", "{\"build_id\":\"b_1\",\"status\":\"ready\"}");
                } else if line.starts_with("PUT") && path.starts_with("/up/") {
                    let url_n: u32 = path[4..].parse().unwrap();
                    let expired = s.expire_after_pieces.is_some_and(|n| s.pieces >= n) && url_n == 1;
                    if expired || url_n != s.url_gen {
                        reply(&mut c, "403 Forbidden", "{\"error\":\"expired\"}");
                        continue;
                    }
                    let have = s.stored.len() as u64;
                    match range.as_deref() {
                        Some(r) if r.starts_with("bytes */") => {
                            let said = if std::mem::take(&mut s.stale_next_probe) { 0 } else { have };
                            reply(&mut c, "200 OK", &format!("{{\"received\":{said},\"complete\":{}}}", have == total));
                        }
                        Some(r) => {
                            let (a, _) = r["bytes ".len()..].split_once('-').unwrap();
                            let a: u64 = a.parse().unwrap();
                            if a != have {
                                reply(&mut c, "409 Conflict", &format!("{{\"received\":{have},\"complete\":false}}"));
                                continue;
                            }
                            s.stored.extend_from_slice(&body);
                            s.accepted += body.len() as u64;
                            s.pieces += 1;
                            if std::mem::take(&mut s.lose_next_reply) {
                                continue; // closes without a reply
                            }
                            let have = s.stored.len() as u64;
                            reply(&mut c, "200 OK", &format!("{{\"received\":{have},\"complete\":{}}}", have == total));
                        }
                        None => {
                            s.stored = body.clone();
                            s.accepted += body.len() as u64;
                            reply(&mut c, "200 OK", "{}");
                        }
                    }
                } else {
                    reply(&mut c, "404 Not Found", "{}");
                }
            }
        });
        base
    }

    fn bundle_file(tag: &str, size: usize) -> (std::path::PathBuf, Vec<u8>) {
        let p = std::env::temp_dir().join(format!("floptle-ship-{tag}-{}.tar.gz", std::process::id()));
        let bytes: Vec<u8> = (0..size).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(&p, &bytes).unwrap();
        (p, bytes)
    }

    fn bundle<'a>(p: &'a Path, size: u64) -> Bundle<'a> {
        Bundle { game: "freeflier", path: p, sha256: "abc", size, engine_version: "0.99.0", label: None }
    }

    fn no_wait(_: u32) -> Duration {
        Duration::ZERO
    }

    #[test]
    fn a_bundle_goes_up_in_pieces_and_is_completed() {
        let (p, bytes) = bundle_file("pieces", 50_000);
        let site = Arc::new(Mutex::new(Site { chunk: Some(8192), ..Site::default() }));
        let base = serve(site.clone(), bytes.len() as u64);
        let mut seen = Vec::new();
        let id = upload_with_token(&base, "tok", &bundle(&p, bytes.len() as u64), &mut |e| { seen.push(e); true }, no_wait)
            .expect("uploads");
        assert_eq!(id, "b_1");
        let s = site.lock().unwrap();
        assert_eq!(s.stored, bytes, "the site's copy differs");
        assert_eq!(s.pieces, 7, "50 000 bytes in 8 KiB pieces");
        assert!(s.completed);
        assert_eq!(seen.last(), Some(&Progress::Completing));
    }

    /// Killed part-way and run again: it asks where the site stands and sends
    /// only what is missing.
    #[test]
    fn a_stopped_upload_carries_on_from_where_the_site_stands() {
        let (p, bytes) = bundle_file("resume", 50_000);
        let site = Arc::new(Mutex::new(Site { chunk: Some(8192), ..Site::default() }));
        let base = serve(site.clone(), bytes.len() as u64);
        let b = bundle(&p, bytes.len() as u64);
        let mut pieces = 0;
        let first = upload_with_token(&base, "tok", &b, &mut |e| {
            if matches!(e, Progress::Sent { .. }) {
                pieces += 1;
            }
            pieces < 2
        }, no_wait);
        assert!(first.is_err(), "the first run was meant to stop");
        assert!(!site.lock().unwrap().completed);

        let mut already = None;
        upload_with_token(&base, "tok", &b, &mut |e| {
            if let Progress::Reserved { already: a, .. } = e {
                already = Some(a);
            }
            true
        }, no_wait)
        .expect("carries on");
        assert_eq!(already, Some(16_384), "the second run did not start from what the site had");
        let s = site.lock().unwrap();
        assert_eq!(s.stored, bytes);
        assert_eq!(s.accepted, bytes.len() as u64, "a byte was sent twice");
        assert!(s.completed);
    }

    /// An expired URL (403) is replaced by reserving again; a lost reply and a
    /// stale answer are recovered by asking; the copy still comes out whole.
    #[test]
    fn an_expired_url_a_lost_reply_and_a_stale_answer_are_all_recovered() {
        let (p, bytes) = bundle_file("recover", 50_000);
        let site = Arc::new(Mutex::new(Site {
            chunk: Some(8192),
            expire_after_pieces: Some(3),
            lose_next_reply: true,
            ..Site::default()
        }));
        let base = serve(site.clone(), bytes.len() as u64);
        let b = bundle(&p, bytes.len() as u64);
        let mut n = 0;
        upload_with_token(&base, "tok", &b, &mut |e| {
            if matches!(e, Progress::Sent { .. }) {
                n += 1;
                if n == 4 {
                    // Next probe claims nothing arrived: the 409 puts it right.
                    let mut s = site.lock().unwrap();
                    s.stale_next_probe = true;
                    s.lose_next_reply = true;
                }
            }
            true
        }, no_wait)
        .expect("recovers");
        let s = site.lock().unwrap();
        assert_eq!(s.stored, bytes, "the site's copy differs after recovering");
        assert_eq!(s.accepted, bytes.len() as u64, "a byte was sent twice");
        assert!(s.url_gen >= 2, "the expired URL was never replaced");
        assert!(s.completed);
    }

    /// A site that offers no pieces cannot take a bundle past Cloudflare's
    /// limit, so nothing is sent and the reason is given.
    #[test]
    fn a_bundle_over_the_single_request_limit_is_not_sent_whole() {
        let p = std::env::temp_dir().join(format!("floptle-ship-big-{}.tar.gz", std::process::id()));
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(SINGLE_PUT_LIMIT + 1).unwrap();
        let site = Arc::new(Mutex::new(Site::default()));
        let base = serve(site.clone(), SINGLE_PUT_LIMIT + 1);
        let e = upload_with_token(&base, "tok", &bundle(&p, SINGLE_PUT_LIMIT + 1), &mut |_| true, no_wait)
            .expect_err("sent a request Cloudflare refuses");
        assert!(e.contains("offered no piece upload"), "{e}");
        assert!(site.lock().unwrap().requests.iter().all(|r| !r.starts_with("PUT")));
        let _ = std::fs::remove_file(&p);
    }
}
