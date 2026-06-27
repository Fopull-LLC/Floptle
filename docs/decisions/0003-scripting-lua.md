# ADR-0003 — Game scripting: Lua (LuaJIT via mlua)

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
Game logic lives in scripts attached to nodes. The developer wants **hot-reload**
iteration (change a script, see it live without recompiling the engine) and to
edit scripts in VSCode (ADR-0011).

## Decision
Embed **Lua** via **`mlua`** with the **LuaJIT** backend as the node-scripting
language.

## Why
- **Fast** (LuaJIT is among the fastest scripting runtimes) and **tiny** — fits
  the lightweight goal.
- **Hot-reload** is simple: re-run a changed file in a fresh environment.
- **Great editor support** in VSCode; opens cleanly as plain text.
- `mlua` is a solid, safe Rust binding with good ergonomics.

## Alternatives considered
- **Rhai** — pure-Rust, tighter type integration, but slower and a smaller
  ecosystem. A reasonable fallback if LuaJIT portability ever bites.
- **C# via .NET hosting** — excellent tooling, but hosting CoreCLR is heavy and
  fights "lightweight"; reintroduces a GC.
- **WASM components** — powerful sandboxing/perf, but more ceremony than node
  scripts warrant today.
- **A custom scripting language** — most control, but a large subproject; the
  shader IR (ADR-0007) is the language-building effort we *do* take on.

## Consequences
- Dynamic typing at the script boundary; mitigated with a clearly-typed,
  curated engine API and Lua LSP type annotations.
- A Rust↔Lua FFI surface to design carefully (`floptle-script::bindings`).
