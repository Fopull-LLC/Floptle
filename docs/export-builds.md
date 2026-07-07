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
- **macOS** cannot be cross-compiled (Apple's SDK license) — run the export
  from an editor on a Mac. Prebuilt per-platform engine binaries + fully
  one-click exports are the Hub pipeline's job (ADR-0021, later).

## Multi-device LAN testing

1. Export (or copy the repo and use `--play`).
2. Copy the build folder to each device — same build/commit everywhere: the
   wire protocol refuses mismatched versions at connect.
3. On the host device: F1 → host via relay (lobby code) or direct
   (`quic://ip:port` needs the host's port reachable; the relay path needs no
   port-forwarding anywhere).
4. On the others: F1 → enter the code (or the address) → join.

## v1 limits (deliberate)

- The binary is the full editor in disguise (~the same size); the slim
  dedicated `floptle-runtime` player + packed/compressed assets come with the
  export phase of the roadmap.
- No icon/branding, no asset obfuscation — playtest builds, not store builds.
- Script errors in a build only surface in the netcode overlay/console
  machinery, not on screen: test in the editor first.
