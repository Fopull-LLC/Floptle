# ADR-0021 — Hub & distribution

- **Status:** Proposed · 2026-07-04
- **Decider:** Ty Johnston (Fopull LLC)
- **Detail:** [Hub & distribution proposal](../hub-proposal.md)

## Context
The engine is only usable in-repo (`cargo run`). To let people *make games* — including
non-Rust developers, on Windows, macOS, and Linux — we need a way to install released
engine versions, keep several side by side, and manage/launch projects pinned to a version.
That's a **Hub** (the Unity-Hub / Godot-project-manager role), and it needs a **release
pipeline** to have anything to install.

## Decision
1. **Build the release pipeline first.** A GitHub Actions workflow on a semver tag builds a
   self-contained bundle per OS (editor + runtime assets + `version.json`), checksums them,
   and publishes them plus a machine-readable `releases.json` manifest to GitHub Releases.
2. **The Hub is an eframe + egui desktop app** (ADR-0004), a new `crates/floptle-hub`
   binary that depends only on light crates (`floptle-scene`), never on render/editor. One
   static binary per OS.
3. **Versions come from a manifest, behind a `VersionSource` trait** — a `GithubReleases`
   impl (the real pipeline) and a `LocalBuilds` dev impl — so the distribution host can
   change without touching the Hub.
4. **Per-user state via the `directories` crate**; installs unpacked under the data dir;
   projects live where the user made them and are referenced by path. A project's engine
   version is pinned in its `project.ron` (`engine_version`, serde-default).
5. **Integrity via SHA-256** in the manifest, verified on download; HTTPS via rustls.

## Why
- A single small launcher is the lowest-friction front door and matches how every
  comparable engine ships; egree/egui reuses our stack and avoids a web runtime.
- Pipeline-first because the Hub is useless without installable artifacts, and the bundle
  layout + manifest are the contract everything else builds on.
- The `VersionSource` seam lets us dogfood against a private repo now and flip to a public
  channel at launch without a rewrite.

## Alternatives considered
- **Tauri / web UI** — heavier, a JS toolchain + web runtime, off-stack.
- **Ship one embedded engine version** — no multi-version management, which is a core ask.
- **`cargo install` / package managers** — excludes non-Rust users and every non-Linux
  distro story; no project manager.

## Consequences
- New engine CLI surface the Hub relies on: `--version`, `--new <path>`, `--migrate <path>`.
- Code signing / notarization is deferred (v1 first-run needs the OS "allow unsigned"
  step); the bundle layout leaves room for it.
- **Open (see the proposal §8):** private-token vs public distribution for the first usable
  Hub; macOS arch coverage; whether Create ships starter templates.
