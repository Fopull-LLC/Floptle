# Exporting a game build

**File ⏵ Export Game…** stamps out a runnable build:

```
MyGame/
  MyGame            (or MyGame.exe — the player, renamed to your game)
  floptle-game.ron  (the manifest: title + project pointer)
  assets/           (your project, minus dot-entries like .floptle caches)
```

**What ships is the player, not the editor.** They are two binaries built from
one engine: the editor you author in, and a player with the whole authoring
half — egui, the dock, the Inspector, the asset browser, the file pickers —
*not compiled into it at all*. On this machine that is 51 MB of editor against
30 MB of player, and the difference is not hidden chrome, it is absent code.

Running that binary IS the game: the manifest next to it names the title and
the assets folder, and it boots straight into the game filling the window. `Esc` releases a captured cursor and a click gives it back (it never quits — and alt-tabbing away releases it too, so a build cannot reach out of an unfocused window and take the pointer); in a browser the same two gestures work, because the browser's own `Esc` exits pointer lock;
**F1 opens the multiplayer menu** — in a build it's the game-facing version
(host → lobby code, join by code, direct address; the editor's simulated-link
test tools don't ship), and a "F1 — multiplayer" hint shows for the first few
seconds. Its relay field reads `cloud` by default: **Host** puts the game on
Floptle Cloud (the build carries the project's game key) and shows a
six-character code, and **Join by code** takes one as typed — the first letter
names the region, so the code is the whole address. A `host:port` in that
field is your own `floptle-relay`, with five-letter codes. Close the window
to quit.

Games can also drive sessions from Lua instead of the F1 menu —
`net.host{ relay = "cloud" }` / `net.join("cloud://" .. code)` from any script
(say, a main-menu controller), or `net.host{ relay = "host:port" }` /
`net.join("relay://host:port/CODE")` through your own relay. A proper in-game
UI system for real menus is on the roadmap; until then F1 is the built-in
fallback.

Player mode is also a CLI flag for quick playtests of a project without an
export: `floptle-editor --play [PROJECT_DIR]`.

## Platforms

The dialog's **Target** picker chooses the build's platform. **Every target
works from every machine** — Windows builds from Linux, Linux builds from a
Mac, macOS builds from Windows. There is no compiler and no toolchain involved.

- **This machine** — instant: the export copies the player binary sitting
  beside your editor (both come out of the same install).
- **Windows (x86_64)**, **Linux (x86_64)**, **macOS (Apple Silicon)**,
  **macOS (Intel)** — the export uses an **engine template**: the release
  bundle the pipeline already publishes for that platform, downloaded once,
  checksum-verified against `releases.json`, and cached at

  ```
  <data-dir>/templates/<engine-version>/<platform>/floptle-player[.exe]
  ```

  (`~/.local/share/floptle/` on Linux, `~/Library/Application Support/Floptle/`
  on macOS, `%APPDATA%\Floptle\` on Windows — beside the Hub's installed
  versions, because a template and an installed engine are the same artifact.)

  The first export of a platform fetches ~15–40 MB and takes a few seconds;
  every export after that is instant.
- **Web (browser)** — the same template mechanism, one more artifact: the
  engine as a WebAssembly module with its page. The build is a **folder you
  serve**, not a program you run — see [Web builds](#web-builds) below.

### Why templates, not compilation

An exported build is *the engine binary + your assets + a manifest*. Nothing
about your project is compiled in — so the binary a build needs isn't something
to produce, it's something to fetch. It is byte-for-byte the bundle the release
pipeline already builds for that platform.

This is how Godot ("export templates") and Unity ("build support modules")
work, and it's why exporting doesn't need what compiling would: the engine
source, a C cross-toolchain, or (for macOS, which cannot be cross-compiled at
all) a second machine.

A template is pinned to the editor's **own version**. Mixing them would ship a
game whose wire protocol disagrees with the editor that built it, so the
version is part of the cache key and a mismatch can't happen silently.

### Building from source

If you run the editor from a source checkout at a version that has no published
bundles yet — engine development between a version bump and its release — the
export falls back to `cargo build --release --target <triple>` for that
platform, and says so. That needs the target and, for Windows, a mingw
cross-toolchain:

```bash
rustup target add x86_64-pc-windows-gnu
# either (portable, no root): unpack llvm-mingw into ~/.local/opt/llvm-mingw
#   https://github.com/mstorsjo/llvm-mingw/releases  (…-ucrt-ubuntu-…-x86_64.tar.xz)
# or system-wide:              pacman -S mingw-w64-gcc   (Arch/CachyOS)
```

macOS has no fallback — Apple's SDK can't leave a Mac. Released versions always
have a macOS template, so this only bites during engine development.

macOS builds ship a `README.txt` for the recipient: the build is unsigned, so
they clear the quarantine flag once (`xattr -dr com.apple.quarantine <exe>`)
before launching. Signing/notarization is a Hub-pipeline concern.

## A Steam game

A project with a Steam App ID (**Project Settings ▸ Game ▸ Steam App ID**) ships the
Steam player and Valve's runtime library beside it, in place of the plain
player. Every other project ships exactly as below, with no Valve binary in it.
The details, including testing an export outside Steam, are in
[Shipping a Steam game](scripting/steam.md#28d-shipping-a-steam-game).

## What ships, and what doesn't

The export owns the `assets/` copy, and deliberately leaves things out:

- **Authoring inputs the engine has no loader for.** The model formats an
  import turns into a `.glb` (`.fbx`, `.obj`, `.mtl`, `.dae`, `.stl`, `.ply`),
  content-tool project files (`.blend`, `.psd`, `.c4d`), another engine's
  artifacts (`.uasset`, `.umap`), and a project's own tooling (`.py`). The
  `.glb` that came *out* of the import ships; the file that went *in* does
  not. Nothing is lost — none of these could be loaded at runtime either way —
  and the export reports the count and the megabytes. On a finished
  first-person game it was **857 files and 45 MB**, most of it asset-pack
  leftovers. `.meta` is *not* on that list: the terrain streamer writes those.
- **dot-entries** (`.floptle` caches, `.luarc.json`) — editor and IDE plumbing.
- **`save/`** — the engine writes player save slots there (`save.set` in Lua).
  Shipping your copy hands every player a pre-populated save and changes what
  the game does on first launch.
- **`replays/`** — recorded match logs.

Only at the project root: a nested folder named `save/` is content and ships.
An exported build writes neither folder into itself; a player's data lives in
a folder of its own, described next.

**Anything else you name in `.floptleignore`.** A file beside `project.ron`,
one pattern per line, read the way `.gitignore` reads them:

```
# folders, at any depth
tools/
docs/
# a leading / means at the project root only
/AGENTS.md
# the masters; the game plays the .mp3 copies
*.wav
audio/**/*.aiff
```

A `#` starts a comment only at the beginning of a line.

`*` and `?` match within a name and `**` matches any number of folders. The
export (native, web and server) reports how many files and megabytes the list
left out.

**The biggest files nothing names.** After the copy, the report lists up to
five of the heaviest images, sounds, models, fonts and videos (1 MB or more)
whose file name appears in none of the build's scripts, scenes or data files.
It is a hint rather than a verdict: a name a script puts together at runtime
(`track .. ".mp3"`) is not written anywhere either. If nothing loads them, add
them to `.floptleignore`.

**Linked packages.** A package you work on through a link is copied into the
build's `packages/` folder, and the build's `packages.ron` lists it as an
ordinary package. A switched-off link is not copied and is taken off the
build's list, so no path on your disk ends up in the build.

**Absolute asset paths are rewritten.** An absolute path is taken as written
when it exists, so a build carrying one is broken on every machine except the
one that exported it — silently, because a missing model simply doesn't
appear. Paths that point *into* the project are made relative automatically
(the export reports how many). A path that points *outside* the project but
names a file the build carries — a reference written where the project used to
live, on another disk or another operating system — is redirected to the
build's own copy, and the report lists each one. A path with no such file in
the build can't be repaired, so it's listed as a warning.

The project itself shouldn't hold them either, because a teammate who opens
it has the same problem a player would. The editor saves every path to a file
inside the project as project-relative, whichever way it got into the scene.
`floptle check` fails on any absolute path it finds in a `.ron` file, naming
the file, line, node and field, and `floptle check --fix` rewrites the ones
inside the project to relative ones. A path outside the project has no
relative form, so `check` says the export won't include that file at all.

The player applies the same rescue at load time: an absolute reference that
names nothing is walked from its tail (`…/MyGame/models/door.glb` →
`models/door.glb`) and the longest tail that exists in the project wins. That
is what keeps a project drawing after it moves folders, disks or machines, but
the export's rewrite is what makes a build say so up front.

The **entry scene** is resolved the way `scene.load` resolves names: a path
(`scenes/menu.ron`) or a bare scene name (`menu`) both work. If it resolves to
nothing the export fails rather than shipping a build that boots somewhere else.

## Where a player's data lives

A player's data outlives the build that wrote it. An exported game keeps its
saves, replays and everything a script writes under `user://` in a per-user
folder, so installing a new version (unzipped beside the old one, updated by
Steam, or re-exported by you) finds everything the last one wrote:

| OS | Folder |
|---|---|
| Windows | `%APPDATA%\<studio>\<data_id>\` (just `%APPDATA%\<data_id>\` with no studio) |
| macOS | `~/Library/Application Support/<data_id>/` |
| Linux | `$XDG_DATA_HOME/<data_id>/`, which is `~/.local/share/<data_id>/` by default |

Inside it are `save/` (the `save.*` slots, and `crash.txt` if the game has
crashed), `replays/`, and `user/` (the files a script writes as `user://…`).
`FLOPTLE_DATA_DIR` set in the environment overrides the folder, for a portable
install. A script can show the path with `app.dataPath()`.

**`data_id` names the folder, and it must not change.** It is in `project.ron`.
The first export writes one made from the title (`"Free Flier"` becomes
`free-flier`) and says so; after that it stays as it is, even if the title
changes, because a new build finds the last one's data by it. Rename it and
every player starts over. `studio`, also in `project.ron`, is the publisher
folder the game's folder sits in on Windows; macOS and Linux do not use one.

**Builds made before this kept their saves beside the game.** The first time a
new build runs, if its data folder is empty and its own folder has a `save/` or
`replays/`, it copies them over, so a player who updates in place keeps their
progress. It copies rather than moves: the install folder may be read-only,
and the old build still finds its files if the player goes back to it. A player
who unzipped the new build somewhere else has their old saves in the old
folder, and nothing reads them from there.

**The editor and the command line keep all of it in the project.** Play in the
editor, `floptle run`, `floptle shot` and `floptle play` use the project's
`save/`, with `user://` at `save/user/`, which the export leaves out. A test run
never touches a player's real files, and a build never ships yours. A browser
build keeps it in the page's storage, as below.

**Steam Auto-Cloud.** Because the folder no longer moves between versions, Steam
can sync it. In the Steamworks Auto-Cloud settings, add one root per platform,
with the path below and pattern `*`, recursive:

| Platform | Root | Subdirectory |
|---|---|---|
| Windows | `WinAppDataRoaming` | `<studio>/<data_id>/save` (or `<data_id>/save`) |
| macOS | `MacAppSupport` | `<data_id>/save` |
| Linux | `LinuxXdgDataHome` | `<data_id>/save` |

Add the same with `user` in place of `save` to sync player-made files too.

## Web builds

**Target ⏵ Web (browser)** stamps a folder that plays in a browser:

```
MyGame-web/
  index.html          the page: a loading bar, a Play button, the game's canvas
  game.flpk           your project, packed into one file the page downloads
  pkg/                the engine — the WebAssembly module and its JS glue
  README.txt          how to serve it
```

Serve the folder over HTTP and open it — a browser will not load a game from a
`file://` URL, so double-clicking `index.html` shows nothing. For a look on
your own machine, `python3 -m http.server 8000` inside the folder and open
`http://localhost:8000/`. For itch.io, zip the folder's *contents* (so
`index.html` is at the top of the zip) and upload it as an HTML project; no
special headers are needed.

What is different from a desktop build, and deliberately so:

- **WebGPU is required.** Current Chrome, Edge and Safari have it; Firefox is
  still rolling it out. The page checks first and says so by name rather than
  showing a black canvas. There is no WebGL2 fallback — the engine's main mesh
  shader cannot be expressed in it ([web-export.md](web-export.md) has the
  table).
- **The whole project downloads before the game starts.** The loading bar
  counts the engine and your project together. There is no streaming in this
  version, so a build's size is a player's wait. The export says how big the bundle came out and which kinds
  of file fill it, e.g.:

  ```
  game.flpk is 294.0 MB (1275 asset file(s)), the engine module 18.5 MB
    — mostly ogg 183.2 MB, png 61.4 MB, glb 23.3 MB
  ```

  That is a real game, and it is too big for the web as it stands. Audio is
  almost always the bulk. Until the export re-encodes it for you (see the
  limits below), the lever is in your project: shorter loops, mono where
  stereo buys nothing, and a lower Vorbis quality when you export the source
  from your audio tool.
- **Saves live in the browser.** `save.*` writes to the page's own storage,
  scoped to the game's title, so a slot survives a reload but stays on that
  machine and browser. Browsers cap this at a few megabytes.
- **Sound starts on a click.** Browsers only allow audio after the player has
  interacted with the page; the Play button is that click.
- **The loading screen stays up until the game is running.** After Play it
  says what it is waiting on (the graphics, opening the game, the world) and
  keeps moving while it waits. It comes down once the game draws smooth frames
  with nothing left to load: terrain still meshing, or models your scripts ask
  for in their first frames, are loaded behind it. A game that streams its
  world without end is shown after 20 seconds.
- **`http.*`, `cloud.*` and `assets.textureFromUrl` work through the page's
  `fetch`**, provided the server allows the page's address (CORS). Floptle
  Cloud allowing web builds is its own step; see
  [web-export.md](web-export.md#networking-in-a-page).
- **Online play joins, it does not host.** A page joins a relay lobby over the
  relay's browser leg (`net.join("cloud://CODE")`, or `wss://relay:port/CODE`
  for your own relay), in the same lobby as desktop players. It cannot host,
  and it cannot join a server directly; both are refused in one sentence.
  `app.isWeb()` tells a script it is in a page, so it can hide those buttons.
- **No Steam, and no voice chat.** Each refuses in one sentence rather than
  hanging.
- **Background work runs on the frame that asked for it.** A navmesh bake or a
  planet being generated stalls the frame it starts on instead of running on a
  thread, because a page has none without headers most hosts do not send. A
  game that relies on those at runtime will hitch there.
- **`backdrop()` UI shaders read black.** Frosting what is behind a panel means
  sampling the image already drawn, and a browser's canvas cannot be sampled.
  The UI draws normally; only the frosted backdrop is missing.

### Not yet, and worth knowing before you plan around it

The export does **not** re-encode your assets. Audio ships at the bitrate you
authored, textures at the size you authored, and scripts as source. Those are
the three levers that would take a large project from a few hundred megabytes
to something a player will wait for, and they are the next piece of this
feature rather than part of it today. The export tells you the numbers so the
gap is visible rather than discovered by a player.

From a source checkout, `tools/web/build.sh` builds the web template the
export uses (it needs the WASI SDK and `wasm-bindgen-cli`, and says so).

## Headless / scripted builds

```
floptle --export <PROJECT_DIR> <OUT_DIR> <PLATFORM> [TITLE]
```

`PLATFORM` is `host`, `web`, or a release artifact key (`windows-x86_64`,
`linux-x86_64`, `macos-aarch64`, `macos-x86_64`). No window, no GPU — same code
the dialog runs, so CI gets exactly the editor's behaviour:

```bash
floptle --export ~/games/MyGame ~/builds/MyGame-win windows-x86_64 "My Game"
floptle export ~/games/MyGame ~/builds/MyGame-web web --title "My Game"
```

## The command line

Every verb below is built and ships in the one `floptle` binary. Most take an
optional `PROJECT` (defaulting to the current directory) and most accept
`--json`, so a script or CI job can read the answer instead of a human reading
the screen. `floptle help <VERB>` explains any one of them.

```
[x] floptle new <DIR> [--template NAME] [--engine-version V] [--no-examples]
[x] floptle templates
[x] floptle open [PROJECT]                     # the bare invocation, said out loud
[x] floptle play [PROJECT]
[x] floptle run [PROJECT] [--scene S] [--frames N | --seconds T] [--seed N] [--timing] [--alloc] [--json]
[x] floptle shot [PROJECT] [--scene S] [--camera NAME] [--size WxH] [--after T] [--frames N --turn DEG] [--no-ui] [--out FILE] [--timing]
[x] floptle vfx [PROJECT] --effect KEY [--at SECS] [--frames N] [--scene S] [--out DIR]
[x] floptle inspect [PROJECT] [--scene S] [--select QUERY] [--json]
[x] floptle check [PROJECT] [--json]
[x] floptle lint [PROJECT] [--vec3] [--json]    # what to change before switching vec3
[x] floptle exec <SCRIPT.lua> [PROJECT] [--json]
[x] floptle api [QUERY] [--json]
[x] floptle export <PROJ> <OUT> <PLATFORM|server> [--title T] [--scene S] [--label L]
                                     # server: name OUT *.tar.gz for the archive
[x] floptle ship <PROJ> [--scene S] [--label L] [--fresh] [--json]
                                     # export a server bundle and upload it to Floptle Cloud
[x] floptle cloud collections <PROJ> [--apply] [--json]
                                     # compare cloud_collections.ron with the server, or apply it
[x] floptle bake gi | clips | nav [ARGS]       # all three headless
[x] floptle migrate <DIR> [--engine-version V]
[x] floptle serve <PROJ> [--port N | --relay URL] [--scene S] [--tick HZ]
[x] floptle doctor [--json]                    # can THIS machine render?
[x] floptle help [VERB] [--json]
[x] floptle version [--json]
```

The older flag forms (`--export`, `--new`, `--migrate`, `--version`,
`--engine-version`) still work and mean the same thing; the subcommands are the
documented spelling.

`shot` and `vfx` are the two that answer "what does it look like" without a
window. `shot` photographs a scene through its camera. `vfx` photographs one
particle effect across its own timeline — a single frame cannot show an effect,
since a burst reads as an empty frame before it fires and as drifting smoke
after it — and tiles the moments into one contact sheet, through a camera fixed
across all of them so they can be compared. Which moments are worth
photographing is decided by rendering the effect at thumbnail size first and
keeping the part where something actually lands in the picture.

`shot --frames N` writes a sequence instead of one picture: the world keeps
playing a fixed step between frames, and each frame is drawn from the camera
the scene holds at that step. Add `--turn DEG` to look around: the camera yaws
by `DEG` over the run and nods from 10° below to 60° above its own forward,
three times. A thing that flickers cannot be shown by one frame; a folder of
them can be scanned for it.

Two diagnostics reach the windowed editor the same way, for a glitch that only
happens on screen. `FLOPTLE_FRAME_DUMP=<dir>` photographs every presented frame
into `<dir>` out of the swapchain image itself, exactly as `--shot` does for
one, and `floptle <project> --auto-play` presses Play as soon as the scene is
up (`FLOPTLE_AUTO_LOOK=scene` or `=game` then keeps that tab in front and
flicks the camera about by itself, for an unattended run). Together they turn "it flashes sometimes while I look around" into a folder
of the frames that were actually on the screen, one of which has the flash in
it. The dump costs a readback per frame and writes about two megabytes per
frame, so it is for a minute of reproducing something, not for playing.

## Multi-device LAN testing

1. Export (or copy the repo and use `--play`).
2. Copy the build folder to each device — same build/commit everywhere: the
   wire protocol refuses mismatched versions at connect.
3. On the host device: F1 → host via relay (a lobby code, on Floptle Cloud or
   your own relay) or direct (`quic://ip:port` needs the host's port reachable;
   the relay path needs no port-forwarding anywhere).
4. On the others: F1 → enter the code (or the address) → join.

## Hosting on a server instead of a player's machine

Peer-hosting is the default and needs nothing beyond the above. It has two
limits that only matter for some games: the world ends when the host closes
their laptop, and the host is also a player, with an unfair zero-latency view
of the simulation everyone else sees over the wire.

The **dedicated server** removes both. It is not a smaller engine: it is the
same `World`, the same physics, the same scripts, the same session and the same
tick the editor runs when you press Play — minus the window, the GPU, the audio
and the input, because nobody is sitting at it:

```
floptle serve <project-dir> [--scene scenes/arena.ron]
              [--port 7777 | --relay host:port] [--tick 60]
              [--interest 150] [--budget 16384]
```

`floptle-runtime --server <project-dir> …` is the older spelling of the same
thing, with the same flags.

| Flag | Meaning |
|---|---|
| `--scene` | the scene to host; defaults to the project's entry scene |
| `--port` | listen for direct QUIC connections on this UDP port |
| `--relay` | register a lobby on a relay instead, so nobody port-forwards |
| `--tick` | simulation rate in Hz (default 60) |
| `--interest` | turn on interest management with this radius in metres |
| `--budget` | per-client snapshot budget in bytes/sec (with `--interest`) |

It ships the project directory as-is — copy the same folder you'd export, and
keep it on the same engine version as the clients (the wire protocol refuses
mismatches at connect).

Two things it deliberately will not do. It **refuses a `Rollback` scene**: a
rollback match has every peer simulating every tick, so its "host" is a referee
and a relay rather than a simulation, and for a fighting game that role is one
of the players. And it does no interpolation, audio or VFX — a server that spent
time on any of it would be spending it on nothing.

Started from a terminal it prints a peer-count heartbeat every 30 seconds and
stops on Enter. Started by a service manager or a container — anywhere stdin
isn't a TTY — the Enter watcher isn't installed at all.

**Stopping is graceful.** On `SIGTERM` or `SIGINT` — which is what `systemctl
stop`, a container runtime and Ctrl-C all send — the server tells every player
the world is going away, gives the message half a second to actually leave, and
exits 0. They see "the server is shutting down" rather than a connection that
dropped for no reason, which is the difference between *they are updating* and
*it broke again*. Your game shows the reason through `net.on("kicked", fn)`.

### `floptle-server`, and why it is a different binary

`floptle serve` is the editor binary wearing a different hat, which is fine on
your own machine and wrong on a server: it links the OS audio and gamepad
libraries, and those are resolved when the process *starts*, not when something
first wants a sound. On a minimal Linux image, which has no `libasound2`, it
does not fail to find an audio device — it fails to start at all.

`floptle-server` is the same engine and the same code with those left out. It
needs nothing beyond the C runtime, so it runs on a stock minimal image with no
packages installed:

```
floptle-server <project-dir> [--scene scenes/arena.ron]
               [--port 7777 | --relay host:port] [--tick 60]
               [--max-players 32] [--status-file /run/floptle/server.json]
```

| Flag | Meaning |
|---|---|
| `--build` | the same thing as the positional, said about an exported server folder |
| `--max-players` | refuse a join past this many players. Nobody already playing is ever dropped for it |
| `--status-file` | write a small JSON document here every 5 seconds: `peers`, `uptime_s`, `ticks`, `tick_hz`, `scene`, `lobby_code`, `tick_p95_ms`, and `game_key_prefix` — the first twelve characters of the key, enough to say which one; the key itself is never written to disk. Written and renamed, so a watcher never reads half a file. The directory has to be one the server's user can write — under systemd, a `RuntimeDirectory=` of its own |
| `--script-budget-ms` | how long one tick's scripts may run before the script that overran is stopped. Default 500 ms on a server (the editor and a player build allow 2 s). The stopped script's error names the budget; it runs again once its file changes |
| `--game-key` | which Floptle Cloud game this process belongs to. Recorded and reported, not checked — a dedicated server is reached directly. Also read from `FLOPTLE_GAME_KEY`, which is how a service manager should pass it: a command line is readable by every process on the machine and is copied into the system log |

See [multiplayer.md §6](multiplayer.md) for the surrounding decisions.

### A server bundle, for a box you do not log in to

Floptle Cloud's dedicated hosting runs your game on a region's box. What you
upload is not a build with a binary in it — the box runs its own
`floptle-server` — it is a **server bundle**: the project, minus everything a
headless run never reads, plus a manifest saying which scene and which engine
version to run it with.

```
floptle export <PROJECT> ~/builds/my-game-server.tar.gz server [--scene scenes/lobby.ron]
```

**Name the output `.tar.gz` and you get the archive itself** — the thing you
upload, with nothing to do in between. Upload it on the game's page at
[fopull.com/cloud](https://fopull.com/cloud), which reads the manifest back and
offers the scene it names.

Name a folder instead and you get a folder: `<OUT>/floptle-server.ron` and
`<OUT>/assets/`, which is useful if you want to look inside one before it goes
anywhere.

**Or export and upload in one step:**

```
floptle ship <PROJECT> [--scene scenes/lobby.ron] [--label "lobby v3"]
```

`ship` makes the same bundle and uploads it to the page of the game your
project is connected to, as the account signed in to the Floptle Hub. The
build then waits on the game's page for you to deploy it. The upload goes in
pieces, so a bundle of any size gets there, and a dropped connection or an
expired upload link is picked up without you doing anything.

If it is stopped part-way (killed, the laptop closed), run the same command
again. While the project is unchanged, the same bundle carries on from where
the site's copy ends. Once you have edited the project, a fresh bundle is made
instead. `--fresh` makes a fresh one regardless, and `--json` answers with one
object for a script. The editor does the same from **⚙ Settings ▸ Networked ▸ Ship
a server build**, with its progress in the Console.

What the export does, so you know what you are shipping:

- **Leaves out what a server cannot use** — textures, audio, fonts, video and
  shaders — and says how much it left out. Models stay, because a mesh collider
  *is* a mesh and a skeleton lives inside the `.glb`; every `.ron`, script and
  text file stays, because a script may read its own data through
  `assets.getContents` and the strip list does not get to guess which file that
  is.
- **Materialises linked packages and makes paths portable**, the same as a
  native export.
- **Refuses a project that cannot run headless**, at your machine with the
  reason in front of you, rather than as a deployment that goes `failed` on a
  box you cannot see. A `Rollback` scene is the common one: every peer simulates
  a rollback match, so it is hosted by a player, and a dedicated server has
  nothing to drive.
- **Refuses to pin an engine version no box can fetch.** The box downloads
  `floptle-server-<version>-linux-aarch64` for the version the manifest names,
  and that is published for **stable releases from 0.85.0** only. A bundle
  exported from a beta build would pin a file that exists nowhere, so the export
  says so instead — export it from the stable engine.
- **Makes every file readable.** The bundle is unpacked by one user and read by
  another; a file that was `0600` on your machine would reach the box as a file
  the server silently runs without. The export normalises the modes (the box
  does too), so nothing depends on your umask.
- **Says which scene it hosts.** `--scene` wins; otherwise the project's entry
  scene. ⚠ It is the scene the server *boots into* — for a game with a lobby
  that swaps to a map, that is the lobby scene, where the persistent nodes that
  run the match live.

The upload ceiling is 256 MB; a real bundle is tens of megabytes, because the
models are usually most of what is left.

Two things a game needs before it runs well on a box: `net.isDedicated()`
([lua-api.md](lua-api.md)), so the server does not seat itself as a player, and
a game key in **Project settings ⏵ Networked** ([multiplayer.md](multiplayer.md)),
so the server registers with the region's relay and gets the six-character code
players join with.

## v1 limits (deliberate)

- Desktop builds ship the project folder as it is — no packing, no
  compression. (The web build packs it into one file, because a page has to
  download it; a desktop build has no reason to.)
- No icon/branding, no asset obfuscation — playtest builds, not store builds.
- Script errors in a build only surface in the netcode overlay/console
  machinery, not on screen: test in the editor first.
