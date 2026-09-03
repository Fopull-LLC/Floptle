# ADR-0027 — A command line an agent can drive

Status: **Accepted** (2026-08-19)
Supersedes: nothing. Extends ADR-0011 (the editor shelling out to a
developer's tools) with the reverse direction: a developer's tools driving the
editor.
Raised by: the CLI design work of 2026-08 — the flags exist, the command line
does not. The inventory of what shipped, and the build order, were working
material and are not published; everything this decision rests on is restated
here. What the command line does today is [the CLI section of
export-builds.md](../export-builds.md).

## Context

Four binaries ship and three of them take arguments, all grown one flag at a
time as the Hub, CI and the export pipeline each needed one. There is no
designed command line.

The consequences are concrete. `floptle --help` lists the editor's flags and
nothing lists the runtime's. Three parsers disagree about unknown arguments,
about `--flag=value`, and about what exits with what code. `--bake-gi` is
documented as "bake … and exit" and in fact falls through to the event loop and
opens the editor. And there is no way to ask a project a question — how many
scenes, which assets are missing, what version it was stamped with — without
opening it, so every answer the editor can compute in a panel is an answer a
script cannot get.

What makes this worth deciding now rather than tidying later is who the second
audience turned out to be. A person can open the editor and look; that is the
whole point of an editor, and it is why the missing command line has cost
surprisingly little so far. An AI assistant working in the same project cannot.
It has a terminal and the files, and everything the editor knows — whether the
scene loads, what the sprite looks like, why the script raised — is behind a
window it has no way to open. It is not slowed down by the gap; it is stopped by
it, and it fails in the worst way available, by guessing.

The engine is unusually well placed to close that gap, and by accident. The
extension host in `crates/floptle-editor/src/ext/` already runs Lua with no
window and no GPU — `ExtHost::new()`, `begin_frame(Snapshot, SceneMirror)`,
`reload()` is what its own test module does — because the design rule there was
that **Lua never touches the editor**: every binding reads a per-frame mirror or
pushes an `ExtCmd` onto a queue the editor drains after the frame. That rule was
adopted so a package could not be holding `&mut Editor` when the editor wanted
it back. It also means the whole editor-scripting API is already a headless API,
and `docs/editor-scripting.md` is already held against the live bindings by a
test.

## The decision

**Six choices, taken together.**

**1. Subcommands on the existing `floptle` binary, and the old flags keep
working.** Not a second binary: the engine ships as one file, the Hub installs
it, an export bundles it, and four of the operations in question already run in
it without a window. A separate CLI would have to reach the same functions, and
the moment it reaches them through a copy rather than a call,
[export-builds.md](../export-builds.md)'s promise — *"same code the dialog runs,
so CI gets exactly the editor's behaviour"* — stops being true. `--export`,
`--new`, `--migrate`, `--version` and `--engine-version` are a shipped interface
with callers we do not control; they map onto the subcommands and stay.

**2. One verb table is the single source of truth.** Not a parser somewhere and
a help string somewhere else, which is how the three current parsers came to
disagree. The table carries, per verb: its flags and their types, its exit
codes, its output schema, whether it needs a GPU, and whether it writes to the
project. The parser, `--help`, and the machine-readable spec are all generated
from it.

**3. `clap`'s builder API parses, driven from that table.** This is the
dependency question the proposal flagged, and the measurement changes the
answer: `env_logger` already pulls the entire `anstream`/`anstyle` colour stack
and serde already pulls `syn`/`quote`/`heck`, so the genuinely new crates are
about five, and clap adds roughly 0.3–0.5 MB to a stripped release binary that
is already 51.5 MB. What it buys is worth more to the second audience than to
the first: `--flag=value`, short bundling, `--` separation, conflicts and
requires, and **did-you-mean on a mistyped flag**. An agent that types
`--platfrom` and gets a correction recovers in one step; the same agent against
a hand-rolled parser gets `unknown argument`, exit 2, and a guess. The *builder*
API rather than the derive, so the table stays the authority and no proc-macro
lands on the slowest crate in the workspace to build.

**4. Every verb answers in JSON on request, and exit codes are stated.** `0`
success, `1` the operation ran and failed, `2` usage, and a verb may document
more. `--json` is cheap to promise now and expensive to retrofit, and the shape
each verb emits is part of the verb table, so `floptle help --json` describes
the output as well as the input.

**5. The CLI describes itself.** `floptle help --json` emits the whole table:
every verb, every flag, every exit code, every output schema, and the metadata
above. An agent reads it once and knows the surface, instead of scraping
`--help` or reading `main.rs`. This is the field that has no home in clap, and
it is the reason the table exists rather than the derive.

**6. A shipped game refuses the developer verbs.** One binary means an exported
game contains the same code, and a `floptle-game.ron` manifest beside the binary
already means "this process is a game" (`export.rs`). That is the gate: with a
manifest present, the developer verbs are not offered and not accepted. A
player's build should no more expose `floptle exec` than it should open the
Inspector.

**MCP is a later phase, deliberately.** A protocol wrapper generated from the
verb table is a small job *once the table exists*, and a second hand-maintained
surface is a large one forever. The CLI is the interface; anything else is
generated from it.

## Why this rather than the alternatives

**Why not hand-rolled, given the repo's dependency posture?** It was the
proposal's own recommendation and it remains defensible — `server.rs` shows a
58-line parser with real validation and tests. But it is 58 lines for six
flags with no boolean among them; it advances two arguments at a time and is
correct only because every flag it has takes a value. A dozen verbs is where
that shape starts accumulating the inconsistencies this ADR exists to remove,
and the thing it cannot buy at any price is a good error message for a caller
that cannot see the source.

**Why not clap's derive?** Because choice 5 is the point. Attributes on a struct
model *how to parse this*; they have nowhere to put "needs a GPU", "writes to
your project", "here is the shape of what comes back". That table would have to
exist alongside the derive anyway, and then two things describe one CLI, which
is the failure being fixed.

**Why not a `floptle-cli` binary, so a game ships nothing extra?** Choice 6 gets
the same result for the cost of one condition, and a second binary doubles what
the Hub installs and what a user puts on `PATH` in order to relocate code that
already lives in the first one.

**Why not leave it as flags and document them better?** Documentation does not
fix three parsers disagreeing about unknown arguments, and it does not give a
caller a machine-readable surface. The proposal's sharpest observation stands:
the verbs live in the wrong noun. `floptle-editor --export` says an editor is
required for a build, which is exactly the thing that is not true.

## Consequences

- **A new dependency lands in the binary the Hub installs and every exported
  game ships.** Small and measured, but it is a standing change to what a
  player's build contains, and this ADR is where that was decided.
- **Adding a verb means adding a row, not writing a parser** — and a row that
  omits its exit codes or its output schema is an incomplete row, which is
  something a test can say.
- **`floptle help --json` becomes an interface with consumers**, so its shape is
  versioned and changing it is a breaking change like any other.
- **The docs-coverage discipline extends to the CLI.** An undocumented package
  binding is already a build failure (`docs/editor-scripting.md`); a verb absent
  from the docs, or a documented verb absent from the table, is the same class
  of failure and gets the same kind of test.
- **The old flags stay until they are deprecated on purpose**, with their own
  decision and their own notice. They are not removed as a side effect of this
  one.
