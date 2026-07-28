# Exporting a game build (v1)

**File ⏵ Export Game…** stamps out a runnable build:

```
MyGame/
  MyGame            (or MyGame.exe — the engine binary, renamed)
  floptle-game.ron  (the manifest: title + project pointer)
  assets/           (your project, minus dot-entries like .floptle caches)
```

Running that binary IS the game: the manifest next to it flips the engine into
**player mode** — it boots straight into Play with the Game view filling the
window, no editor chrome. `Esc` releases a captured cursor (it never quits);
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

The dialog's **Target** picker chooses the build's platform:

- **This machine** — instant: the export copies the running binary itself.
- **Windows (x86_64)** from Linux — the export compiles the engine for
  Windows in the background (`cargo build --release --target
  x86_64-pc-windows-gnu`, spawned for you; the dialog spinner runs until it
  lands, first build takes minutes, incremental rebuilds are quick). Needs the
  target + a mingw cross-toolchain once:

  ```bash
  rustup target add x86_64-pc-windows-gnu
  # either (portable, no root): unpack llvm-mingw into ~/.local/opt/llvm-mingw
  #   https://github.com/mstorsjo/llvm-mingw/releases  (…-ucrt-ubuntu-…-x86_64.tar.xz)
  # or system-wide:              pacman -S mingw-w64-gcc   (Arch/CachyOS)
  ```

  Cross exports need the engine source checkout the editor was built from
  (it rebuilds itself) — i.e. a dev machine, which is where exports happen.
- **macOS** — Apple's SDK can't leave a Mac, so GitHub's macOS runners build
  the engine binary natively and the export consumes it:

  1. Push the repo, then GitHub ⏵ **Actions ⏵ macos-binary ⏵ Run workflow**
     (`arm64` default = Apple Silicon; `universal` also covers Intel Macs at
     ~2× the minutes — note macOS runner minutes bill at 10× on private repos,
     which is why it's on-demand).
  2. Download the `floptle-macos` artifact, untar, put the binary at
     **`prebuilt/floptle-macos`** in this checkout (git-ignored).
  3. Export with Target = macOS — instant from then on; refresh the prebuilt
     when you want the build on a newer engine commit (the wire protocol
     refuses version mismatches at connect, so keep it current for
     multiplayer tests).

  macOS exports include a `README.txt` for the recipient: the build is
  unsigned, so after downloading they run
  `xattr -dr com.apple.quarantine .` once in the folder, then launch the
  binary from Terminal. (Signing/notarization is a Hub-pipeline concern.)

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

- The binary is the full editor in disguise (~the same size); the slim
  dedicated `floptle-runtime` player + packed/compressed assets come with the
  export phase of the roadmap.
- No icon/branding, no asset obfuscation — playtest builds, not store builds.
- Script errors in a build only surface in the netcode overlay/console
  machinery, not on screen: test in the editor first.
