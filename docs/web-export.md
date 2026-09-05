# Web export

**File ⏵ Export Game… ⏵ Web (browser)** stamps a folder that plays in a
browser; [export-builds.md](export-builds.md#web-builds) is the page for using
it. This page is the story behind it — what a browser build is and is not,
the change to the scripting engine it required, and the evidence each decision
was made on — kept because the decisions still hold and will be asked about.

The short version: it targets WebGPU, it required one change to the scripting
engine, and that change was worth making even for a game that never leaves the
desktop. Nothing you had written needed rewriting.

---

## What it will be

- **WebGPU.** Modern Chrome, Edge and Safari 18+; Firefox as it finishes
  shipping. Every shader the engine has runs there unmodified.
- **A player build, not the editor.** The same thing **File ⏵ Export Game…**
  makes today ([export-builds.md](export-builds.md)), with a browser as one more
  target: an `index.html`, a loading bar, and your game on a canvas. The editor
  stays on the desktop.
- **Sized for itch.io.** A download bar is part of the game, so the export does
  real work on your assets rather than zipping them and hoping.

## What it will not be

**WebGL2.** "WebGL export" is what people say when they mean "in a browser", and
it is by far the more expensive of the two. Four of the engine's shaders cannot
be expressed in it at all — including the main one that draws every mesh in your
game:

| Shader | Why WebGL2 refuses it |
| --- | --- |
| The mesh raster path | Reads six storage buffers — GPU skinning, vertex paint and the extended material data. WebGL2 has no storage buffers at all. |
| 2D lighting | Reads the depth buffer directly. |
| Post-processing (depth of field, motion blur) | Same. |
| Ambient occlusion | Same. |

Supporting it would mean re-expressing all GPU skinning and all vertex paint as
textures — every animated character in every game — for a browser share that is
shrinking every quarter. **The decision on record: WebGPU only. A WebGL2
fallback is declined at a cost of four shader modules including the main raster
path, and will be revisited only if a real population of players demands it.**

Also out of scope for the first version: the editor in a browser, threads (so
no cross-origin isolation headers to fight with your host), Steam, gamepads via
`gilrs`, and HTTP requests from scripts. The last three will say so out loud
rather than hanging — an unavailable feature that fails silently is worse than
one that isn't there.

---

## The part that affects you today: the scripting engine

Floptle scripts ran on LuaJIT until v0.84.0. **LuaJIT cannot run in a
browser** — its interpreter is hand-written assembly, one version per processor
architecture, and wasm is not one of them. This is not a gap somebody could fill
in a weekend; there is no port to fund.

So the engine changed Lua flavour: **Luau**, everywhere — desktop and web, one
engine to test, one set of behaviour to document. **This shipped in v0.84.0**;
the rest of this section is the reasoning behind it, and what it means for your
scripts. It is the Lua dialect Roblox
built and maintains, it is closer to the Lua 5.1 your scripts are already
written in than Lua 5.4 is, and it compiles to the browser with a stock
toolchain.

**It is also faster at the thing this engine actually does.** Scripts here spend
their time crossing between Lua and the engine — reading a node's position,
multiplying a vector, setting it back. Measured on exactly that shape, one
million operations:

| | LuaJIT | Luau | Luau, native vectors |
| --- | --- | --- | --- |
| Vector maths through the engine | 1180 ms | 537 ms | **65 ms** |

LuaJIT loses because every crossing breaks the traces it exists to build. Pure
number-crunching in a tight Lua loop is the other way round — LuaJIT is faster
there — which is why the change ships behind a benchmark on real games, not a
microbenchmark. If a real game gets slower, this stops.

**It did not get slower.** Two finished games, benchmarked headless on release
builds of both engines, alternating between them so neither got the warmer
machine — the worst frame in twenty, in real milliseconds:

| | LuaJIT | Luau |
| --- | --- | --- |
| A solar-system builder, 56 scripts | 4.81 ms | **4.15 ms** |
| A first-person game, 38 scripts, 330 MB of assets | 13.99 ms | **12.38 ms** |

Luau came out ahead in every single pair, on a build with its code generator
switched off — so that is the floor, not the ceiling. The tight-numeric-loop
case where LuaJIT wins is real, and it did not show up in either game.

### Nothing you have written needed rewriting

That was a commitment rather than a hope, and it is how the work was sequenced.
All three of these are **done, in v0.84.0** — the rest of this page marks which
parts have shipped and which are still plan:

1. The engine swapped to Luau with **vectors exactly as they were** — same type,
   same precision, still writable field by field. No script changed. The 2.2×
   arrived on its own.
2. Before that swap shipped, a harness ran the entire scripting test suite,
   every shipped game script and every tutorial project **on both engines** and
   compared the results. A difference was a bug to fix, not a note to add to the
   release.
3. Anything Luau genuinely lacked that a real script used got a replacement or a
   loud warning naming the line. Never a silent difference.

### Then, per project: `exact` or `fast` — shipped in v0.84.0

The third-column number above — 65 ms, and no garbage collected at all — comes
from Luau's native vector type, which is three 32-bit floats living inside the
language rather than an object on the heap. That is a genuine trade, because
Floptle positions are 64-bit on purpose: a solar-system-scale game needs the
precision, and a 32-bit float has lost centimetre accuracy by ~131 km from the
origin.

So it is a project setting — **Project Settings ▸ Scripting ▸ Script vec3** —
and both answers stay supported:

- **`exact`** — vectors as they are today. 64-bit, writable, correct at any
  distance from the origin.
- **`fast`** — Luau native vectors. Immutable, 32-bit, no allocation and no
  garbage collector pause. Roughly 18× faster on a vector-only micro-benchmark;
  a whole game moves far less, because vectors are one allocator among many —
  `floptle run --alloc` measures yours, script by script.

**New projects start on `fast`. Every existing project is pinned to `exact` the
first time it opens, and stays there until you change it yourself.** Nothing
about your game changes because you updated the engine. When you do opt in,
`floptle lint --vec3` reads your scripts and hands you the list of lines to
touch — and in `fast`, a vector component that wanders past the precision cliff
warns once, by name, rather than quietly drifting.

Two things are worth doing in your own scripts either way, and both work in
`exact` today:

```lua
-- instead of mutating in place:
v.x = 5

-- build a new one:
v = v:withX(5)
```

This is the direction the docs teach from here on. If you write vectors this
way, `fast` is a one-line project setting and not a migration.

---

## Payload: the part that decides whether anyone plays it

A desktop player waits for a download because they chose to install something. A
browser player waits for the download *in the game*, and leaves.

One of the projects driving this work is a first-person horror game that ships
about 330 MB of assets — 175 MB of it audio, 50 MB of textures. No amount of
renderer porting makes that a web build. So the export pipeline is part of the
feature, not a follow-up:

- **Audio re-encoded to a lower Vorbis quality preset.** This is the big one:
  audio is more than half the payload of a real project.
- **Textures audited, with an optional maximum dimension.** Modern compressed
  texture formats are a bigger feature and will be their own, later.
- **Scripts precompiled**, so the game starts sooner and ships no source.
- **A size pass on the engine itself**, measured and published in the release
  notes rather than estimated here.

The target is a project of that size under about 80 MB. That number will be
reported honestly when it is measured, including if it is missed.

---

## How it lands

In order, and each step was useful on its own. **Steps 1–6 have shipped;
step 7's browser probe runs by hand, not yet in CI.**

1. ~~**The Luau engine**, at full parity, behind a build switch.~~ **Shipped in
   v0.84.0.**
2. ~~**Luau becomes the default.**~~ **Shipped in v0.84.0.** LuaJIT stays
   buildable for one release as an escape hatch, then goes.
3. ~~**`exact` / `fast` vectors**, the lint, and the immutable helpers.~~
   **Shipped in v0.84.0**, and the rough edges around the setting were fixed in
   v0.84.1.
4. ~~**The engine running in a browser** — a skinned, vertex-painted character
   through the real render graph, at retro resolution, with a frame-time
   readout. This is the go/no-go moment for the web half, and it is a
   screenshot, not a spreadsheet.~~ **Passed, 2026-09-04.** See the next
   section: the go was a go, and it found three shader bugs on the way.
5. ~~**The platform edges**: assets from a preloaded bundle instead of the
   disk, saves in browser storage, audio through WebAudio.~~ **Shipped.** The
   engine reads its files through one seam (`std::fs` on the desktop, the
   bundle in a page), measures time with a clock that exists in both, and
   runs its background jobs inline where there are no threads. The CI lint for
   the browser target refuses a direct disk or clock call in the engine, so
   this cannot quietly come undone.
6. ~~**`export --target web`**.~~ **Shipped** — the dialog's Web target and
   `floptle export … web`, with the template fetched and cached like any
   platform's. The asset pipeline (re-encoding audio, downscaling textures,
   precompiling scripts) is not part of it yet: a build downloads the project
   as it is.
7. **Verification and release**: the browser probe (`tools/web/shot.py`) runs
   on a machine with a browser; a CI job with one is still to come.

Steps 1–3 were worth doing whether or not the browser half ever shipped, and
the engine has been faster for them since. Step 4 was where the honest
uncertainty was concentrated, and it was answered before anything was built on
it.

**What the export target waited on**, kept for the record. Two things had to
happen before the dialog listed a browser. The engine had to *compile* for it
— everything that only exists on a desktop (the FBX importer's C library,
QUIC, the OS keyring, blocking HTTP, the clipboard, the file manager, the
dedicated server) behind a target gate with a browser answer or a one-sentence
refusal. And then it had to *run* there, which compiling says nothing about: a
disk read compiles for the browser and fails on the page, a clock read
compiles and panics. So every file the engine reads goes through one seam that
is `std::fs` on the desktop and the preloaded bundle in a page; saves go to the
page's storage through the same seam; the frame clock is one that exists on
both; and each background job runs inline where there is no thread to hand it
to. The rule that keeps it that way is a lint on the browser target, in CI,
that refuses a direct disk, clock, thread or process call in the engine — the
gate is a build failure rather than a bug report from a tab.

Signing in on a page is still not available, and that one is not the engine's
to fix: a page may only talk to fopull.com once fopull.com allows the game's
origin and offers a sign-in a page can redirect to.

---

## Step 4: the engine in a browser tab

A wasm module — `crates/floptle-web`, built by `tools/web/build.sh` — runs a
four-rung ladder on a page, each rung a line a headless browser can read:

1. **The scripting engine.** Luau in a tab: eval, `pcall` recovering from an
   error raised in Rust and one raised in Lua, a syntax error reported rather
   than trapped, native vectors, two hundred thousand tables allocated and
   collected, an uncaught error survived. All eight checks pass.
2. **A WebGPU device** through the renderer's own device path, awaited rather
   than blocked on — the only difference from the desktop.
3. **Every shader module through the browser's own compiler**, then the real
   raster pass with a mesh carrying vertex paint and a skin — the two
   storage-buffer paths WebGL2 could not have carried. All eleven modules
   compile.
4. **Frames.** The skinned, painted bar curling at 240 rows, upscaled into a
   640×360 canvas through the engine's own retro path and post chain, at the
   display's 60 Hz. The picture came back from the GPU and it is the bar.

The browser build weighs **2.4 MB of wasm** before any size work — the
scripting engine, the whole renderer, and the C++ runtime it needed.

### What it found

The claim on this page was "every shader ships as-is on WebGPU". It was almost
right, and the difference is worth knowing about because it is invisible from
the desktop. wgpu on a desktop validates WGSL with its own compiler; a browser
validates with *its* compiler, which enforces two rules the desktop one lets
through. Three shader modules were refused — the mesh raster path, the post
chain and the game UI — for:

- **A texture sampled from non-uniform control flow.** Sampling with implicit
  derivatives is only legal where every pixel in a group takes the same path.
  The depth prepass took an early `return` on per-instance data and sampled
  after it; the depth-of-field and motion-blur loops ran after a per-pixel
  early return; the UI shader sampled inside branches on the element kind.
  Fixed by sampling first or by asking for level 0 explicitly; nothing
  rendered by a desktop changes, and the desktop probes for cutout, skinning,
  paint, depth of field, motion blur and UI text all still pass.
- **`*` and `^` mixed without parentheses**, which WGSL forbids, in the UI
  grain hash.

Two more things the desktop never sees:

- **A browser canvas offers no sRGB surface format.** The renderer now views
  the swapchain through an sRGB view where the surface itself has none, so
  linear light reaches the screen encoded the same way it does on the desktop.
- **The C++ runtime in a Rust wasm module.** Luau raises errors with C++
  exceptions; Rust's browser target ships no C++ runtime; the WASI SDK's does
  (version 33 or newer), with its C++ compiled for a different wasm target and
  linked into this one. The dozen system calls its libc then wants are
  answered in Rust, so the finished module imports nothing from WASI. And the
  linker, seeing no reference to the constructor list, wraps every exported
  function in a call to it — every call from the page re-ran every C++ static
  constructor until the module referenced the list itself. That one cost an
  afternoon and is written down in the crate.

### A build is a real game binary now

One thing had to change before a browser target could exist at all, and it is
worth its own note because it improves every desktop build too.

**An export used to ship the editor.** The engine was one program, and a
"build" was that program with a manifest beside it that hid the authoring
chrome — so every game you shipped carried an Inspector, an asset browser, a
dock, a code editor and a set of OS file dialogs that the player could never
open. A browser has an answer for none of those, so the web target was blocked
behind the same wall.

Floptle now builds **two binaries from one engine**: the editor you author in,
and a player with the authoring half *not compiled into it*. Measured on the
machine this was written on, in release:

| | editor | player |
| --- | --- | --- |
| binary | 51.2 MB | **29.9 MB** |
| crates compiled in | 535 | **415** |
| egui in the dependency graph | yes | **none at all** |

**File ⏵ Export Game… ships the player**, and the engine bundle published for
each platform now carries both, so cross-platform export is unchanged: pick a
target, get that platform's player. Nothing about how you export is different.
A bundle from before the split has no player in it, and an export against one
says so by name rather than quietly shipping the editor again.

For the browser this is the difference between "port the editor" and "compile
the game", and it is the piece step 5 was really waiting on.

### Reproducing it

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version <the version Cargo.lock pins> --locked
tools/web/build.sh          # fetches the WASI SDK on first use
tools/web/shot.py --display # runs the page in a browser, prints the ladder,
                            # writes target/web/probe.png
```

Headless Chromium runs the whole ladder but aborts the readback that makes
the picture; `--display` opens a window, which completes it, and closes it
again. CI builds and links the module on every push, and checks that its
import list names nothing from WASI.

---

## Would it actually run?

For a game shaped like the ones people build in Floptle: the GPU is not what
would stop it.

A retro-resolution game — the `retro_height` setting in Project Settings, at,
say, 445 rows — renders its whole 3D scene into roughly 350,000 pixels, about a
sixth of 1080p, and scales that up. On top of that the engine asks a graphics
card for nothing unusual: no optional GPU features are required, there is not a
single compute pass in the entire engine, there is no multisampling to
renegotiate, and the crate that does the drawing never touches the filesystem.
Low-poly art at a sixth of 1080p is close to the friendliest case a browser GPU
gets.

The honest expectation is that download size, not frame rate, is what makes or
breaks a web build. That is why the pipeline above is in the plan from the start
rather than bolted on.

---

## The evidence

Everything above was measured against the engine at v0.83.0 on 2026-09-01, not
estimated. This section is for a reader who would rather check than take it on
trust; the file references are into this repository.

| Claim | How it was established |
| --- | --- |
| LuaJIT cannot target the browser | `lua-src`'s build script has exactly one wasm arm (emscripten) and panics for `wasm32-unknown-unknown`. |
| Luau does compile for the browser | `mlua 0.10` with `features = ["luau"]` builds clean for `wasm32-wasip1` in 37 s with a stock wasi-sdk. The link step needs one prepared artifact — see the appendix. |
| WebGL2 costs four shaders, WebGPU costs none | A naga probe over all 11 shader modules at `Version::Embedded { version: 300, is_webgl: true }`. `raster.wgsl` refuses on storage buffers, non-perspective interpolation, runtime-sized arrays and texture-level queries; `light2d`, `post` and `ssao` refuse on loading from a depth texture. |
| Luau is faster on this engine's hot path | 1M operations against the engine's real vector type — userdata wrapping a 3×`f64`, metamethods implemented in Rust. Numbers in the table above. Pure numeric loops go the other way: 14 ms LuaJIT against 35 ms Luau with its code generator. |
| The migration surface is small | No LuaJIT-only feature (`ffi`, `jit.*`) appears anywhere in the scripting host, the shipped game scripts, or the documentation. All 503 engine-to-Lua call sites go through an abstraction the flavour does not reach. And **no first-party `.lua` file in the engine uses a name the two Lua flavours disagree about** — not `bit.*`, not `loadstring`, not `goto`, not `debug.getinfo` — checked across all 99 of them, and pinned by a test that runs under both. |
| Vectors are mutable today, and positions are genuinely 64-bit | Field setters at `crates/floptle-script/src/math_api.rs:58`; `TransformDoc.translation: [f64; 3]` at `crates/floptle-scene/src/lib.rs:749`. |
| The renderer asks for nothing exotic | `required_features` resolves to profiling queries only where the adapter already offers them, and to nothing at all headless (`crates/floptle-render/src/device.rs:176`, `:207`, `:334`). Twelve shader modules, no compute passes, `sample_count: 1` throughout. |

### What the migration has found so far

The scripting engine has been ported and runs under both Luas, and the
compatibility work turned up exactly the kind of thing this section exists to
report:

- **The whole scripting test suite — 330 tests — passes identically under
  both.** So do the runtime and the editor, which now build against either.
- **One genuine bug, and it was in the engine already.** Clearing a tile with
  the engine's own "empty" value went through a conversion that is 64-bit on
  LuaJIT and 32-bit on Luau; on Luau it quietly became a different tile number
  and reported success. Fixed by carrying the value in a form both Luas agree
  about. It is the one class of problem this migration was most at risk of
  adding, so it is worth saying plainly that it was found by a test rather than
  by a player.
- **One limit disappears.** LuaJIT refuses to load a script whose functions
  close over more than 60 file-scope `local`s — a wall that has cost this
  project two releases in one long controller file. Luau has no such limit;
  measured, not assumed, at sixty-eight times that. The engine's warning about
  it goes quiet rather than pointing at a wall that is not there.
- **Error messages got better, on both.** The two Luas describe a mistake
  differently — one names the field you misspelled, the other names the field
  you read from it — and neither names the whole thing. So the engine now reads
  your script and says both: *"`node.postion` is nil, and `.x` was read from
  it"*, with the line quoted underneath. A misspelled field name is the
  commonest mistake there is, and this is the first release where the message
  points straight at it. It does not guess: where a line is ambiguous it shows
  you the line and names nothing.
- **`goto` and `bit` go.** They are LuaJIT extensions; Luau replaces `bit` with
  `bit32` and has no `goto`. No engine script or template uses either, but if
  yours does, that is the list. Runtime code loading (`loadstring`) goes too — a
  sandbox that can compile a string is not one.

Two limits are known and not yet resolved, and are named here rather than
discovered later: the engine requests the default device limits, which WebGPU's
own defaults are expected to satisfy but which will be intersected with the
adapter's if they do not; and GPU timestamp queries are commonly unavailable in
browsers, which the engine already degrades gracefully for.

---

## Appendix — reproducing the spike

The load-bearing claim is "Luau compiles to wasm". Reproduce it before building
on it.

```toml
[dependencies]
mlua = { version = "0.10", features = ["luau"] }
```

```sh
rustup target add wasm32-wasip1
W=/path/to/wasi-sdk-25.0-x86_64-linux
CC_wasm32_wasip1=$W/bin/clang CXX_wasm32_wasip1=$W/bin/clang++ \
AR_wasm32_wasip1=$W/bin/llvm-ar \
cargo build --target wasm32-wasip1        # clean, ~37 s cold
```

The **link** step additionally needs a C++ standard library built with
WebAssembly exceptions. Luau's parser throws internally by design — nothing
escapes into Rust, but the symbols must resolve — and the stock wasi-sdk
`libc++abi.a` is built without them:

```sh
CXXSTDLIB_wasm32_wasip1=c++
RUSTFLAGS="-L native=/a/dir/holding/ONLY/libc++.a/and/libc++abi.a -C link-arg=-lc++abi"
```

Give the linker a directory containing *only* those two archives: pointing it at
a whole sysroot shadows Rust's own wasi libc and breaks thread-local setup in a
way that reads as an unrelated failure.

Recorded so nobody re-litigates them, two negative results: PUC Lua 5.4 with
vendored sources panics for `wasm32-unknown-unknown`, and Luau built for that
same target with borrowed WASI headers stops at a `#error` guard in
`<wasi/api.h>`. Both are why the browser build compiles its C++ for
`wasm32-wasip1` and links the objects into a `wasm32-unknown-unknown` module,
satisfying the dozen remaining system imports from JavaScript.
