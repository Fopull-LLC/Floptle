# Running a Floptle Cloud region

This is the operator's page for `floptle-fleet` — the agent that turns a
region's deployments into running dedicated servers. If you are a developer
shipping a game, you want [multiplayer.md](multiplayer.md) and
[export-builds.md](export-builds.md); nothing here is something you have to
know.

One box runs one agent. The agent runs one `floptle-server` per deployment.

```
  fopull.com                        the region's box
  ──────────                        ────────────────
  GET  /fleet/<region>/desired  ─▶  fetch bundle, verify, unpack
                                    write one systemd unit per deployment
  POST /fleet/<region>/status   ◀─  what each one is actually doing
```

## What it does, every ten seconds

1. **Asks what should be running.** `GET /desired` returns one row per
   deployment: the build to run, its SHA-256, the engine version it was built
   against, the port, the scene and the plan's memory cap.
2. **Makes sure it has the pieces.** The bundle is fetched, its digest checked,
   and only then unpacked — bundles are content-addressed by digest, so
   redeploying a build the box already has costs nothing. The pinned engine
   version is downloaded the same way if the box does not have it.
3. **Reconciles the units.** Starts what should be running, stops what should
   not, and leaves alone anything already correct.
4. **Says what happened.** `POST /status` with each deployment's state, player
   count, uptime, restarts, p95 tick time and its last 200 journal lines.

Step 4 is not just reporting. **The control plane holds a stopped deployment's
UDP port for five minutes after the agent reports it gone**, because handing a
live port on while the old server's players are still sending packets would
deliver their traffic into a different game. If status reports stop landing,
ports stop being released.

## Setting up a box

You need three things on it: the binary, a token, and a unit.

### 1. The binary

Every release publishes `floptle-fleet-<version>-linux-aarch64` and
`floptle-server-<version>-linux-aarch64` beside the ordinary bundles.

```bash
VER=0.85.0
BASE=https://github.com/Fopull-LLC/Floptle-releases/releases/download/v$VER
curl -fsSLO "$BASE/floptle-fleet-$VER-linux-aarch64"
curl -fsSLO "$BASE/floptle-fleet-$VER-linux-aarch64.sha256"
sha256sum -c "floptle-fleet-$VER-linux-aarch64.sha256"
sudo install -m755 "floptle-fleet-$VER-linux-aarch64" /usr/local/bin/floptle-fleet
floptle-fleet --help
```

That last line is worth running: it prints a table and exits 0. A binary that
treats `--help` as an argument is an old one.

You do **not** need to install `floptle-server` by hand — the agent downloads
whichever version each bundle pins, and verifies its published checksum before
running it. Versions accumulate under `/var/lib/floptle-fleet/engines/`, which
is deliberate: two deployments can pin two different engines, and a developer
who has not re-exported must not be silently upgraded by somebody else's deploy.

### 2. The token

The token is minted **on the box**, over SSH, and never leaves it. It is a
credential for the whole region — anything that can present it can claim to be
this box.

```bash
sudo install -d -m700 /etc/floptle
sudo tee /etc/floptle/fleet-token >/dev/null   # paste, then Ctrl-D
sudo chmod 600 /etc/floptle/fleet-token
```

⚠ **Do not `chmod 644` this file.** The unit below runs with
`DynamicUser=yes`, which cannot read a root-owned `0600` file, and loosening the
mode is the fix people reach for — it hands the region's secret to every user on
the machine. `LoadCredential` exists precisely so the service can be
unprivileged *and* the file can stay unreadable: systemd reads it as root and
hands the service a private copy.

### 3. The unit

```ini
# /etc/systemd/system/floptle-fleet.service
[Unit]
Description=Floptle Cloud fleet agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/floptle-fleet --region us-east --relay us-east.relay.fopull.com:7788
LoadCredential=fleet-token:/etc/floptle/fleet-token
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal

# The agent writes unit files and calls systemctl, so it is root. What it runs
# is not: every game server it starts gets DynamicUser=yes and its own confined
# unit — see `crates/floptle-fleet/src/unit.rs`.
StateDirectory=floptle-fleet
RuntimeDirectory=floptle-fleet
NoNewPrivileges=yes
ProtectHome=yes

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now floptle-fleet
journalctl -u floptle-fleet -f
```

The agent finds the token at `$CREDENTIALS_DIRECTORY/fleet-token` without being
told where it is, which is why the unit needs no `--token-file`.

⚠ **`--relay` is load-bearing.** It is passed through to every server the agent
starts, and it is what makes a dedicated server reachable by the same
six-character code a player uses for a friend's laptop. Without it a server is
reachable only at `quic://host:port`, which no player types. Use the region's
own relay from `GET /cloud/regions`.

**Each server gets a directory of its own under `/run/floptle-d/`** (the
`--run` parent; it must be under `/run`). Its unit declares that directory as
its `RuntimeDirectory=`, so systemd creates it owned by the server's own
dynamic user, and the server's `--status-file` is written inside it — that
file is where the player count, uptime, p95 tick time and **lobby code** in
every status report come from. It is deliberately not under the agent's own
runtime directory: systemd removes a unit's runtime directory when that unit
stops, so an agent restart would otherwise delete every running server's
status file and the portal would show zeros with nothing in the journal to say
why.

### Checking it before you commit to it

```bash
sudo floptle-fleet --region us-east --token-file /etc/floptle/fleet-token \
     --once --dry-run
```

`--dry-run` says what it would do and touches nothing; `--once` does a single
cycle and exits. Together they are a safe first contact with a live control
plane.

## When something is wrong

**"the control plane refused this box's token (401/403)"** — the token is not
for role `fleet`, or it names a different region than `--region`. A box may only
speak for its own region. This is a configuration problem and it will not fix
itself.

**"could not reach the control plane"** — a bad minute at the website. The agent
changes nothing on the box and tries again next cycle. Every running server
keeps running. This is deliberate: treating "I could not ask" as "nothing should
be running" would take a whole region down on a transient error.

**A deployment stuck in `failed`** — read its own journal:
`journalctl -u floptle-d-<deployment_id>.service`. The last 200 lines are also
in the portal, because they are what the agent reports. Two failures the agent
catches before the server ever starts, and reports with a reason rather than a
crash loop:

- the bundle names a `project` directory it does not contain
- the bundle's scene is not in the bundle

**A deployment that keeps restarting** — the unit gives up after 5 starts in 300
seconds and settles in `failed`, on purpose. A build that cannot start should
end up somewhere a developer can see a reason, not restart forever writing the
same traceback.

**`iptables -S` looks like there is no firewall** — read past the first line.
The Oracle boxes open with `-P INPUT ACCEPT` and end with a blanket `REJECT`.
The region's UDP port range has to be open explicitly.

## What the agent will not do

- **It will not run bytes it has not verified.** A bundle is downloaded to a
  temporary file, hashed, and only unpacked if the digest matches what the
  control plane named. The same applies to an engine binary, against its
  published checksum.
- **It will not let a bundle write outside its own directory.** Any archive
  entry that is absolute or climbs with `..` stops the unpack and names the
  entry. Symlinks are skipped.
- **It will not put a game key on a command line.** Keys go into the unit's
  environment, because an `ExecStart` is readable by every `ps` on the box and
  is echoed into the journal — which the agent then ships to the control plane.
- **It will not touch units it does not own.** It manages exactly
  `floptle-d-*.service` in its unit directory.
- **It will not trust a bundle's mode bits.** `tar` carries the developer's
  umask faithfully, and a `0600` file that was fine on their laptop is
  unreadable to the server, which runs as a different user — a server that
  starts, registers, takes a lobby code and runs without its input bindings,
  with nothing to say so. Everything unpacked is made readable (`chmod -R
  a+rX`, in effect); an exec bit that was set is kept.
