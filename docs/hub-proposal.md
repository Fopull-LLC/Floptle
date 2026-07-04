# Floptle Hub & Distribution — design proposal

- **Status:** Draft for review · 2026-07-04
- **Author:** Ty Johnston (Fopull LLC)
- **Companion ADR:** [0021 — Hub & distribution](decisions/0021-hub-and-distribution.md)

## 1. What we're building and why

Today the engine is used *in-repo*: you clone, `cargo run`, and edit. There is no
notion of released versions, no way for someone who isn't a Rust developer to install
the engine, and no way to keep several projects on different engine versions. That's
fine for building the engine; it's a wall for anyone trying to **make a game with it.**

The **Hub** is the front door. One small cross-platform app that:

1. **Installs & manages engine versions** — download a release, keep several side by side.
2. **Manages projects** — a home screen of your projects, each pinned to an engine version.
3. **Creates projects** — pick a name, a location, and a version; get a ready-to-edit project.
4. **Upgrades projects** — move a project to a newer engine version, safely.
5. **Launches the editor** — open a project with exactly the engine version it's pinned to.

This is the Unity-Hub / Godot-project-manager role. It is deliberately **not** the editor:
it never loads a scene or a renderer. It's a launcher + version manager + project registry.

The engine must work on **Windows, macOS, and Linux**, so the Hub and the release
artifacts are cross-platform from day one.

## 2. The two halves

There are really two deliverables, and the Hub depends on the first:

- **A. The release pipeline** — how an engine version becomes a downloadable, verifiable
  artifact per OS, described by a manifest the Hub can read. *We build this first* (per the
  decision below): without it the Hub has nothing to install.
- **B. The Hub app** — the desktop program that reads the manifest, installs versions,
  and manages/launches projects.

## 3. A — the release pipeline

### 3.1 Trigger and build

A GitHub Actions workflow (`.github/workflows/release.yml`) fires on a semver tag
(`v*`). A build matrix produces one self-contained **bundle** per target:

| Target key          | OS build     | Archive   |
|---------------------|--------------|-----------|
| `linux-x86_64`      | ubuntu       | `.tar.gz` |
| `macos-aarch64`     | macos (arm)  | `.tar.gz` |
| `macos-x86_64`      | macos (intel)| `.tar.gz` |
| `windows-x86_64`    | windows      | `.zip`    |

Each bundle contains the editor binary, the bundled runtime assets it needs, and a
`version.json` (`{ "version": "0.3.0", "target": "linux-x86_64", "commit": "…" }`).

### 3.2 The manifest

The Hub never scrapes the GitHub UI. It fetches one **`releases.json`** from a stable URL
(a release asset on a fixed `manifest` tag, or GitHub Pages). Schema:

```json
{
  "schema": 1,
  "channels": { "stable": ["0.3.0"], "beta": ["0.4.0-rc1"] },
  "versions": [
    {
      "version": "0.3.0",
      "channel": "stable",
      "date": "2026-07-04",
      "notes_url": "https://…/releases/tag/v0.3.0",
      "min_project_schema": 3,
      "artifacts": {
        "linux-x86_64":  { "url": "https://…/floptle-0.3.0-linux-x86_64.tar.gz",  "sha256": "…", "size": 48213456 },
        "macos-aarch64": { "url": "…", "sha256": "…", "size": … },
        "windows-x86_64":{ "url": "…", "sha256": "…", "size": … }
      }
    }
  ]
}
```

The CI job regenerates `releases.json` on every release (append the new version, keep the
last N per channel) and uploads it alongside the artifacts.

### 3.3 Integrity & trust

- **SHA-256** for every artifact in the manifest; the Hub verifies after download and
  refuses a mismatch.
- **HTTPS** everywhere (rustls — no system OpenSSL, so the Hub stays a single static binary).
- **Code signing / notarization** (Windows Authenticode, macOS notarization+stapling) is
  *out of scope for v1* but the bundle layout leaves room for it. Until then, first-run on
  macOS/Windows needs the usual "allow unsigned app" step — documented, not automated.

### 3.4 Private vs public — the one real open question

The repo is private and pre-release, and GitHub Release assets on a **private** repo need
an auth token to download. Two viable shapes:

- **Internal/testing now:** artifacts on the private repo's Releases; the Hub carries an
  optional personal-access token (stored in the OS keyring, never on disk in plaintext).
- **Public at launch:** a public release channel (public repo mirror, or an object-storage
  bucket / CDN) so anyone can install with no token.

The `VersionSource` abstraction (§4.4) makes this a config swap, not a rewrite. **Decision
needed:** which is the target for the first usable Hub. Recommendation: wire the
private-token path now (so we can dogfood), design the manifest URL to be swappable to a
public host later.

## 4. B — the Hub app

### 4.1 Tech

- **eframe + egui** (ADR-0004 already chose egui for the editor). A single-window app, one
  static binary per OS, no web runtime, and we reuse the team's egui muscle memory.
- A new workspace binary crate **`crates/floptle-hub/`**. It depends only on light crates
  (`floptle-scene` to read a `project.ron`), **never** on the render/editor crates — the
  Hub must stay small and fast to launch.
- Blocking HTTP (`ureq` + rustls) on background threads, progress streamed back over a
  channel — no async runtime needed for a handful of downloads.
- Cross-platform paths via the `directories` crate; secrets via `keyring`.

### 4.2 Where things live (cross-platform)

`directories::ProjectDirs::from("com", "Fopull", "Floptle")`:

| | data dir | config |
|---|---|---|
| Linux   | `~/.local/share/floptle/` | `~/.config/floptle/` |
| macOS   | `~/Library/Application Support/com.Fopull.Floptle/` | same |
| Windows | `%APPDATA%\Fopull\Floptle\data\` | `…\config\` |

```
<data>/
  versions/<version>/…        # unpacked engine bundles (editor + assets + version.json)
  cache/<file>.tar.gz         # downloaded archives (for verify + resume)
  hub.json                    # projects registry + installed index + settings
```

`hub.json`:

```json
{
  "settings": { "channel": "stable", "default_version": "0.3.0", "manifest_url": "…" },
  "projects": [
    { "name": "My Game", "path": "/home/ty/games/mygame", "engine_version": "0.3.0", "last_opened": "2026-07-04T10:00:00Z" }
  ],
  "installed": ["0.3.0"]
}
```

Projects live **wherever the user made them** — the Hub only references them by path and
re-validates existence on load (a moved/deleted project is flagged, not lost).

### 4.3 The engine-version pin

Each project already has a `project.ron` (the editor's project config). We add an
`engine_version: Option<String>` field (`#[serde(default)]`, so old projects still load).
The Hub reads it to know which install to launch, and writes it on create/upgrade. `hub.json`
caches it for the home screen but `project.ron` is the source of truth (survives a registry
reset / moving the project to another machine).

### 4.4 Version-source abstraction

```rust
trait VersionSource {
    /// Available versions from the manifest (network for GitHub, filesystem for local).
    fn available(&self) -> Result<Vec<ReleaseInfo>>;
    /// The artifact for a version on THIS platform (target key resolved from the OS/arch).
    fn artifact(&self, version: &str) -> Option<Artifact>;
}

struct GithubReleases { manifest_url: String, auth: Option<Token> } // the real pipeline (§3)
struct LocalBuilds  { repo_path: PathBuf }                          // dev: cargo build + package
```

`GithubReleases` is the primary source; `LocalBuilds` lets an engine dev test the whole Hub
flow against a `cargo build`-produced bundle without cutting a release.

### 4.5 Crate layout

```
crates/floptle-hub/
  main.rs        # eframe app, three tabs: Projects · Installs · Settings
  config.rs      # hub.json + directories paths + keyring token
  registry.rs    # Project / Install models, load/save, existence re-validation
  releases.rs    # VersionSource trait + GithubReleases + LocalBuilds, manifest parse
  install.rs     # download → sha256 verify → unpack → progress (worker thread + channel)
  launch.rs      # spawn the editor for a project (per-OS specifics, §4.6)
  ui/{projects,installs,settings}.rs
```

### 4.6 Launching the editor

The editor already opens a project when given a project root. Per OS:

- **Linux:** `versions/<v>/floptle-editor <project_path>`
- **Windows:** `versions\<v>\floptle-editor.exe <project_path>`
- **macOS:** `open -a "<v>/Floptle.app" --args <project_path>` (or the inner Mach-O directly)

The Hub spawns it detached and returns to the home screen (optionally minimizing).

### 4.7 Core flows

1. **First run** — nothing installed → a single "Install latest (stable)" call to action →
   download + verify + unpack → ready.
2. **Create project** — name + location + version (default: newest installed). Scaffold via
   the engine's own project seeding (`floptle-editor --new <path>`, a small flag we add) so
   the Hub never hard-codes the project layout; write `engine_version`; register it.
3. **Open** — launch (§4.6).
4. **Upgrade** — pick a newer installed version; warn + offer a zip backup; set
   `engine_version`; optionally run `floptle-editor --migrate <path>` (format migrations are
   already the engine's job — see the vfx clip-emit migration). `min_project_schema` in the
   manifest guards against opening a too-new project on an old engine.
5. **Manage versions** — list installed + available (from the manifest); install / uninstall
   / set default.

## 5. Engine-side changes this needs

Small, additive, mostly CLI surface:

- `project.ron` gains `engine_version: Option<String>` (`#[serde(default)]`).
- The editor grows three flags used by the Hub:
  - `--version` — print the embedded version and exit (verify installs).
  - `--new <path>` — scaffold a fresh project and exit (Hub's "Create").
  - `--migrate <path>` — run format migrations non-interactively and exit (Hub's "Upgrade").
- A **packaging** target: the release CI must emit a self-contained bundle (editor +
  runtime assets + `version.json`). This is the first concrete work item (§3.1).

## 6. Security & failure handling

- Every download is SHA-256-verified against the manifest; partial/corrupt downloads are
  discarded and retried.
- Network is optional after install — the Hub works offline for launching/managing existing
  projects; only "check for updates / install" needs the network.
- A missing/edited install is detected (verify `version.json`) and offered for repair.
- Token (private-repo auth) lives in the OS keyring, not `hub.json`.

## 7. Phased roadmap

| Phase | Deliverable |
|---|---|
| **0** | This proposal + ADR-0021 (decisions). ← *you are here* |
| **1** | **Release pipeline**: bundle packaging + `releases.json` + `release.yml` CI on tag; engine `--version` + bundle layout. |
| **2** | **Hub skeleton**: eframe app, config/paths, projects registry, open/launch existing projects. |
| **3** | **Installs**: fetch manifest, download + verify + unpack, set default, uninstall. |
| **4** | **Create & upgrade projects**: `--new`/`--migrate`, `engine_version` pin, upgrade+backup. |
| **5** | **Polish**: channels (stable/beta), settings, project thumbnails, offline cache, first-run onboarding. |

## 8. Decisions requested before Phase 1

1. **Distribution target for the first usable Hub** — private-repo + token (dogfood now) vs
   a public channel at launch. (Recommendation: token now, swappable manifest URL.)
2. **macOS builds** — ship both `aarch64` + `x86_64` from the start, or Apple-silicon only
   first? Notarization can wait, but that changes first-run friction.
3. **Project templates** — is "empty project" enough for v1's Create, or do we want a couple
   of starter templates (e.g. first-person, top-down)?

Nothing in Phases 1–5 is blocked on the aesthetic answers; these only shape scope at the edges.
