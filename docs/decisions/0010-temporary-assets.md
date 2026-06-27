# ADR-0010 — Temporary Ocarina of Time test assets

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
To build nostalgic test content quickly, the developer has Ocarina of Time
textures available. These are **copyrighted Nintendo assets** and cannot ship.
The project intends to become public/open-source later.

## Decision
- OoT textures are **local-only placeholders**, kept under
  `assets/textures/_oot_temp/`, which is **git-ignored**.
- They are **never committed** — so they can't leak into history that would be
  painful to scrub when the repo goes public.
- All such assets are **replaced with original Fopull art before any public
  release** (a Phase-10 gate; see ROADMAP).

## Why
- Avoids copyright exposure and keeps git history clean for the future OSS
  release. Scrubbing committed binaries from history is error-prone; not
  committing them is the safe default.

## Consequences
- The engine must not hard-depend on those specific files; default content is
  produced by the developer (built-in textures/materials/shaders) before launch.
- A short "drop test textures here" note lives in `assets/textures/README.md`.
