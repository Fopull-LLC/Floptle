# ADR-0028 — One script VM everywhere (Luau), and a WebGPU-only browser target

Status: **Accepted** (2026-09-01)
Decider: Ty Johnston (Fopull LLC)
Supersedes: ADR-0003 (Lua via mlua, **LuaJIT** backend) — the choice of Lua
and of `mlua` stands; the backend changes.
Depends on: ADR-0002 (wgpu is what makes a browser backend a configuration
rather than a port) and ADR-0015 (f64 world space, which is what makes the
vector question a real trade).
Raised by: the survey of 2026-09-01 — "a Floptle game cannot run in a browser,
and the blocker is the script VM, not the renderer" — and the scoping spike that
followed it. The phase-by-phase build order is working material and is not
published; the public statement of the direction is
[web-export.md](../web-export.md).

## Context

A game shipped from this engine runs on four desktop platforms and cannot run in
a browser. The survey that asked why found the asymmetry that decides
everything: **the renderer is close to ready and the script VM is a wall.**

The renderer asks a GPU for nothing unusual — timestamp queries only where the
adapter already advertises them, nothing at all headless
(`floptle-render/src/device.rs:176`, `:207`, `:334`), zero compute passes in the
whole workspace, `sample_count: 1` everywhere, and not one filesystem call in
the crate that draws.

LuaJIT, meanwhile, cannot reach wasm at all. Its interpreter is hand-written
DynASM assembly per architecture; wasm is not one of them and there is no port
to fund. `lua-src`'s build script has exactly one wasm arm — emscripten — and
panics for `wasm32-unknown-unknown`.

That is a target-triple conflict, and it is load-bearing in a way that is easy
to miss. wgpu's first-class wasm target is `wasm32-unknown-unknown`; its
emscripten support is partial and **WebGL-only**. So "keep PUC Lua, build with
emscripten" is not a scripting decision that leaves the renderer alone — it
silently decides the graphics backend too, and decides it the expensive way.

Three things were measured before deciding, not argued:

1. **WebGL2 costs four shader modules; WebGPU costs none.** A naga probe at
   `Version::Embedded { version: 300, is_webgl: true }` over all 11 modules:
   `raster.wgsl` — the path that draws every mesh in every game — refuses on
   storage buffers, non-perspective interpolation, runtime-sized arrays and
   texture-level queries; `light2d`, `post` and `ssao` refuse on depth-texture
   `textureLoad`. The six storage buffers carry GPU skinning, vertex paint and
   the extended material data. On WebGPU every shader ships as written.
2. **Luau compiles to wasm with a stock toolchain.** `mlua 0.10` with
   `features = ["luau"]`, `wasm32-wasip1`, wasi-sdk-25: clean in 37 s.
3. **Luau is *faster* than LuaJIT on this engine's real hot path.** 1M
   operations against the exact `LuaVec3` shape in use today — userdata wrapping
   a `DVec3`, metamethods in Rust: LuaJIT 1180 ms, Luau 537 ms, Luau native
   `vector` **65 ms with zero collected garbage**. LuaJIT loses because every
   operation crosses the C API and aborts its traces, and this engine's scripts
   live on that boundary. The reverse holds for pure numeric loops in Lua
   (14 ms vs 35 ms with Luau's code generator), which is a real caveat and is
   handled as a gate, not a footnote.

The migration surface was inventoried rather than assumed: no LuaJIT-only
feature (`ffi`, `jit.*`) appears in the scripting host, in shipped Lua, or in
the docs; all 503 `mlua` call sites are VM-abstracted; and across all 99
first-party `.lua` files the only Luau-incompatible standard-library use is
`io.open` / `io.popen` / `setfenv` in six developer-only test scripts.

## The decision

**Three choices, taken together, because each one forces the next.**

**1. One VM everywhere: Luau, native and web.** Not LuaJIT on the desktop and a
second VM in the browser. Two VMs is two dialects to document, two sets of
behaviour to test forever, and a class of bug that only exists on the platform
with the least tooling. The measured hot-path win means this is not a tax paid
for portability — it is a gain that happens to also be portable.

**2. WebGPU only. A WebGL2 fallback is declined**, at a cost of four shader
modules including the main raster path, and will be revisited only if a real
population of players demands it. Supporting WebGL2 means re-expressing all GPU
skinning and all vertex paint as textures or uniform buffers — every animated
character in every game — to reach browsers whose share is shrinking. Priced
honestly as a separate, later, much larger job rather than folded in as "and
also WebGL2".

**3. `vec3` changes in stages, per project, and never silently.** The VM swap
ships with the vector type *untouched* — f64 userdata, still mutable, zero
script breakage — and is worth doing on that basis alone. A later release adds a
per-project setting, `script_vec3: "exact" | "fast"`:

- `exact` — today's f64 userdata. Correct at any distance from the origin.
- `fast` — Luau's native f32 vector. Immutable, no allocation, no collector
  work, ~18× on vector-heavy scripts.

**New projects default to `fast`; an existing project is auto-pinned to `exact`
the first time it loads.** Solar stays `exact` — its system-scale coordinates
are precisely the case f32 cannot serve.

## Why this is a trade and not a free win

Three f64s do not fit a VM value slot. Not in Luau, not anywhere: there is no
"fast f64", ever. And this engine's positions are genuinely 64-bit on purpose
(ADR-0015; `TransformDoc.translation: [f64; 3]`). So precision and zero-cost
vectors cannot both be properties of one type, and any decision that claims
otherwise is deferring the discovery rather than making it.

The per-project split is the same shape Godot ships as its single- and
double-precision builds, which is the evidence that it is livable rather than a
novelty.

## What this obligates

- **No existing project's scripts need rewriting.** Enforced in this order:
  the VM swap ships with `vec3` untouched; a **dual-VM diff harness** — the
  whole scripting suite, every shipped game script, every tutorial project, run
  under both VMs and diffed — gates the default flip; `fast` is opt-in with a
  lint that names the lines; and anything Luau genuinely lacks that first-party
  code used gets a shim or a loud warning, **never a silent difference**. The
  bug ledger is 43% silent failures; this migration must not add to that shape.
- **A bench on real games is the arbiter, not a microbenchmark.** Frame p95
  under both VMs on a scripted scene. A regression on a real game is a
  stop-and-report. **Measured 2026-09-01 and passed**: Solar's system scene
  4.81 ms → 4.15 ms, a Forgery-shaped first-person scene 13.99 ms → 12.38 ms
  (frame p95, median of 5 interleaved release runs, `scripts/scene-bench.sh`),
  with Luau ahead in every one of the ten pairs and with its code generator
  *off*. The synthetic probe's one LuaJIT win — a 2000-iteration numeric loop —
  appears in neither game.
- **A deprecation window.** LuaJIT stays buildable for one release after the
  default flips — the escape-hatch release — and is announced as such.
- **A precision guardrail in `fast`**: a once-per-script warning when a
  component passes ~2^17, where f32 granularity crosses a centimetre at meter
  units. Loud, not silent.
- **`docs/web-export.md` is the public statement**, and it is written as a
  promise about compatibility. It is a commitment, not marketing copy.

## Alternatives considered

- **PUC Lua 5.4 via emscripten.** Compiles, but drags in emscripten, which
  drags in wgpu's GL backend, which means WebGL2, which costs the four shader
  modules above. The scripting choice would have silently decided the renderer.
  Also a bigger dialect jump for existing scripts than Luau is: Luau's 5.1
  heritage is *closer* to what the engine's scripts are written against than 5.4
  is.
- **Keep LuaJIT on native, a second VM on the web.** Rejected: two dialects
  documented and tested forever, and the divergence would be discovered by
  players on the platform with the worst debugging story.
- **LuaJIT FFI cdata for the vector problem** (card `floptle/0176`'s original
  fix). It is the right answer inside a LuaJIT-only world and it dies in a wasm
  one. Superseded by Luau's native vector, which is faster, allocates nothing,
  and is portable to every target. **Do not build it** — noted on that card.
- **A pure-Rust VM (piccolo, hematita).** A rewrite of the entire host binding
  surface rather than a swap, and piccolo is incomplete. Listed for
  completeness.
- **Not shipping a browser target at all.** Genuinely acceptable, and it is why
  the plan is sequenced so the first three phases — Luau at parity, the default
  flip, the vector modes — pay for themselves on the desktop even if the browser
  half is parked. The wasm bring-up is a deliberate, early go/no-go gate rather
  than the foundation everything else sits on.

## Consequences

- `mlua` stays; ADR-0003's reasoning about Lua, hot-reload and VSCode is
  unchanged and unchallenged. Only the backend moves.
- The engine gains one build dimension for a release (`vm-luajit` /
  `vm-luau`, mutually exclusive, both in CI), and loses it again when LuaJIT
  goes.
- The 60-upvalue ceiling a large game script ran into was a LuaJIT limit.
  Luau's is different and must be measured with a deliberate over-limit script,
  not taken from a sentence.
- A browser build has no threads, no `ureq`, no Steam and no gamepads in v1, and
  each of those must fail with a message rather than hang.
- The editor is explicitly **not** in scope for the browser. Player exports only.
