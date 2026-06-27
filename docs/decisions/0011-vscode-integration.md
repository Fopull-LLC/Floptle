# ADR-0011 — "Open in VSCode" workflow

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The developer wants: select a script (or a shader) in the editor → VSCode opens
with the **project directory as the workspace root** and the **selected file
focused** as the active editor tab.

## Decision
Shell out to the VSCode CLI:

```
code <projectRoot> --goto <file>:<line>
```

- `<projectRoot>` opens (or reuses) the project as the workspace folder.
- `--goto file:line` focuses the specific file (and line, when we have one).
- Applies to Lua scripts (`.lua`) and textual shaders (`.flsl`, ADR-0007).

## Why
- Meets the workflow exactly with a one-line integration; no embedded code editor
  to build and maintain (that would be scope creep against "lightweight").
- Reuses an already-open VSCode window for the project, so it feels seamless.

## Alternatives considered
- **Embedded text editor** in the engine — large effort, worse than VSCode.
- **Generic `xdg-open`/file association** — wouldn't set the workspace root or
  jump to a line.

## Consequences
- Requires the `code` CLI on PATH; the editor exposes a configurable "external
  editor command" so other editors (or a custom path) work too.
- On export/runtime this integration is editor-only (no dev dependency shipped).
