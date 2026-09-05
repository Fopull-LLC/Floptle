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
the assets folder, and it boots straight into the game filling the window. `Esc` releases a captured cursor (it never quits);
**F1 opens the multiplayer menu** — in a build it's the game-facing version
(host → lobby code, join by code, direct address; the editor's simulated-link
test tools don't ship), and a "F1 — multiplayer" hint shows for the first few
seconds. Close the window to quit.

Games can also drive sessions from Lua instead of the F1 menu —
`net.host{relay="…"}` / `net.join("relay://…/CODE")` from any script (say, a
main-menu controller). A proper in-game UI system for real menus is on the
roadmap; until then F1 is the built-in fallback.

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

**Absolute asset paths are rewritten.** An absolute path is taken as written
when it exists, so a build carrying one is broken on every machine except the
one that exported it — silently, because a missing model simply doesn't
appear. Paths that point *into* the project are made relative automatically
(the export reports how many). A path that points *outside* the project but
names a file the build carries — a reference written where the project used to
live, on another disk or another operating system — is redirected to the
build's own copy, and the report lists each one. A path with no such file in
the build can't be repaired, so it's listed as a warning.

The player applies the same rescue at load time: an absolute reference that
names nothing is walked from its tail (`…/MyGame/models/door.glb` →
`models/door.glb`) and the longest tail that exists in the project wins. That
is what keeps a project drawing after it moves folders, disks or machines, but
the export's rewrite is what makes a build say so up front.

The **entry scene** is resolved the way `scene.load` resolves names: a path
(`scenes/menu.ron`) or a bare scene name (`menu`) both work. If it resolves to
nothing the export fails rather than shipping a build that boots somewhere else.

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
- **The whole project downloads before the game starts.** The loading bar is
  that download. There is no streaming in this version, so a build's size is a
  player's wait. The export says how big the bundle came out and which kinds
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
- **No networking, no Steam, no `http.*`.** Each refuses in one sentence
  rather than hanging — the same as the desktop's rules for a feature that is
  not there.
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
[x] floptle new <DIR> [--template NAME] [--engine-version V]
[x] floptle templates
[x] floptle open [PROJECT]                     # the bare invocation, said out loud
[x] floptle play [PROJECT]
[x] floptle run [PROJECT] [--scene S] [--frames N | --seconds T] [--seed N] [--timing] [--alloc] [--json]
[x] floptle shot [PROJECT] [--scene S] [--camera NAME] [--size WxH] [--out FILE] [--timing]
[x] floptle vfx [PROJECT] --effect KEY [--at SECS] [--frames N] [--scene S] [--out DIR]
[x] floptle inspect [PROJECT] [--scene S] [--select QUERY] [--json]
[x] floptle check [PROJECT] [--json]
[x] floptle lint [PROJECT] [--vec3] [--json]    # what to change before switching vec3
[x] floptle exec <SCRIPT.lua> [PROJECT] [--json]
[x] floptle api [QUERY] [--json]
[x] floptle export <PROJ> <OUT> <PLATFORM> [--title T]
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

## Multi-device LAN testing

1. Export (or copy the repo and use `--play`).
2. Copy the build folder to each device — same build/commit everywhere: the
   wire protocol refuses mismatched versions at connect.
3. On the host device: F1 → host via relay (lobby code) or direct
   (`quic://ip:port` needs the host's port reachable; the relay path needs no
   port-forwarding anywhere).
4. On the others: F1 → enter the code (or the address) → join.

## Hosting on a server instead of a player's machine

Peer-hosting is the default and needs nothing beyond the above. It has two
limits that only matter for some games: the world ends when the host closes
their laptop, and the host is also a player, with an unfair zero-latency view
of the simulation everyone else sees over the wire.

The **dedicated server** removes both. It is the same `World`, the same
physics, the same scripts and the same session the editor hosts — minus the
window, the GPU, the audio and the input, because nobody is sitting at it:

```
floptle-runtime --server <project-dir> [--scene scenes/arena.ron]
                [--port 7777 | --relay host:port] [--tick 60]
                [--interest 150] [--budget 16384]
```

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
isn't a TTY — the Enter watcher isn't installed at all, so it runs until the
process is signalled. Shutdown is not graceful: clients see the connection drop
rather than a goodbye, which is the right trade for v1 but worth knowing before
you restart a live world.

See [multiplayer.md §6](multiplayer.md) for the surrounding decisions.

## v1 limits (deliberate)

- Desktop builds ship the project folder as it is — no packing, no
  compression. (The web build packs it into one file, because a page has to
  download it; a desktop build has no reason to.)
- No icon/branding, no asset obfuscation — playtest builds, not store builds.
- Script errors in a build only surface in the netcode overlay/console
  machinery, not on screen: test in the editor first.
