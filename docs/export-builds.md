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
**F1 opens the multiplayer menu** (host / join by lobby code / direct address —
the same 🌐 panel as the editor), so any build can host or join LAN and relay
sessions out of the box. Close the window to quit.

Player mode is also a CLI flag for quick playtests of a project without an
export: `floptle-editor --play [PROJECT_DIR]`.

## Platforms

A build targets **the platform the exporting editor runs on** — the export
copies the running binary itself. Windows build → run the export from the
editor on Windows; same for macOS/Linux. (Everything is plain Rust; the editor
builds on all three. Prebuilt per-platform engine binaries + one-click
cross-platform exports are the Hub pipeline's job — ADR-0021, later.)

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
