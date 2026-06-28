# floptle-proof — Beat 1: "Am I Dreaming?"

The go/no-go proof slice. A **standalone, hardcoded-WGSL binary** with **no engine**
(no ECS, RON, shader IR, physics, gravity, or large-world frames) — just the thing
that has to land first: pixels that make someone ask *"what IS that?"*

What it does:
- Raymarches a **time-morphing Mandelbox** at **half resolution** into an HDR target.
- A **feedback/swirl post pass** smears the previous frame into melting dream-trails.
- A **present pass** upscales with ACES tonemap + chromatic aberration + vignette +
  dither + a faint retro scanline.
- A **free-fly camera** so you can drift *into* a lobe and watch it churn.
- **FPS / frame-time in the title bar** (the day-one profiler).

## Run

```bash
# from the repo root
cargo run -p floptle-proof --release
```

(Use `--release` — the raymarcher is much smoother optimized.)

## Controls

| Input | Action |
|---|---|
| **W A S D** | move (forward / left / back / right) |
| **E / Q** | move up / down |
| **Mouse** | look (click the window first to capture; **Esc** releases) |
| **Arrow keys** | look (no capture needed) |
| **Shift** | boost speed |
| **R** | reset camera |
| **Esc** | release mouse, or quit if already released |

## Beat 2 — "Stand in the Dream" (`--bin walk`)

A second proof slice (sibling binary, Beat 1 left untouched) for the **SDF-first
physics** thesis (ADR-0012 / ADR-0014): a kinematic capsule **walks on a morphing
fractal planetoid**, colliding against the field's own distance function, with
**SDF-surface gravity** defining "down" so you can run up the shifting walls.

The design was vetted by an adversarial panel that discovered the Mandelbox is a
great thing to *look at* and a terrible thing to *walk on* (empty interior, ~86°
normal flips per step). So it's **render-detailed / collide-smooth**: the eye sees
a fractal crust, but the feet collide against an explicitly-designed **smooth,
solid macro field** (core sphere + blended hills) that is genuinely walkable
(measured: solid interior, real horizon, ~3° normal rotation/step, `|∇f|≈1`). The
**anti-trapping** rule (analytic surface-velocity carry `df/dt = ∇f·∂w/∂t` +
clamped, momentum-free depenetration) makes a rising wall *lift* the player
instead of swallowing them.

```bash
cargo run -p floptle-proof --bin walk --release
```

| Input | Action |
|---|---|
| **W A S D** | walk on the surface (camera-relative, projected onto the ground) |
| **Space** | jump (with coyote time) |
| **Shift** | sprint |
| **Mouse** | orbit camera (click to capture, **Esc** releases) |
| **Scroll** | zoom in / out; zoom all the way in for first person |
| **F** | reset camera to default distance |
| **R** | respawn above the planet |

The planet is a big solid sphere with blended hills and two **swirling branch
landmasses** that helix up off the surface into the sky (you can walk those too).
The player capsule has a **red clown nose** on its upper-front so you can read
its facing and which way is "up" at a glance.

The title bar is the HUD: fps, camera mode, `grounded?`, `f` (signed distance at
the feet — stays `≥ -radius`, i.e. never embedded), and `vsurf` (surface speed
under you — the money shot is standing still on a heaving wall and riding it).

## Beat 3 — "Descent into the Fractal Core" (`--bin descent`)

**The map is an actual morphing MANDELBULB** — a real 3D fractal you walk on. The
Mandelbox is gorgeous but mathematically un-walkable (chaotic normals, no solid
interior — measured); the Mandelbulb is the fractal that's *both* beautiful and
walkable (measured: ~10° normal rotation/step, `|∇f|≈0.8`, a real 11% solid
interior, and its surface normal is ~radial). That last fact is the key: **gravity
by mass** — pulling toward the fractal's core — gives a stable "up", so you cling
to the actual fractal surface and lean *up its lobes* instead of tumbling. As it
morphs, a time-difference surface-carry **offsets you cleanly** with the shift.

You spawn on a **moon** with the fractal planet on the horizon; walk/jump off and
gravity hands you to the planet. A **grappling hook** (click) and a deliberately
**no-air-control** platformer (coyote + jump buffering + asymmetric jump arc +
squash-and-stretch) round it out, plus **noclip (V)** to fly around and inspect.

> Honest status: this is the **walk-on-a-real-fractal** milestone. True *infinite
> descent into the core* is the next focused step — a bounded fractal has a solid
> center you can't pass on foot; the seamless-zoom-into-detail descent is being
> built on top of this.

```bash
cargo run -p floptle-proof --bin descent --release
```

| Input | Action |
|---|---|
| **W A S D** | walk on the surface (grounded only — no air control) |
| **Space** | jump (coyote time + buffering + variable height) |
| **Shift** | sprint |
| **Mouse** | look (click to capture; click again to fire/hold the grapple) |
| **Scroll** | zoom; all the way in for first person |
| **V** | toggle **noclip free-fly** (Space up / Shift down) — fly into the core |
| **F / R** | reset camera distance / respawn |

You spawn on a **moon** with the bounded fractal **planet** on the horizon; jump/
walk off toward it and gravity hands you to the planet. The planet is **nested
solid shells** (radius 32 / 16 / 8 / …) self-similar forever inward — use **V
noclip** to fly through them and watch the infinite descent. Visuals are
deliberately **stripped to clean geometry + lighting + palette** (no feedback
trails / chromatic aberration / noise) so the raw fractal is legible.

> Experimental cut. Known next step: the shells are *complete* spheres, so on-foot
> descent needs **porous shells**; for now fly in with noclip to inspect. Feel
> (gravity, jump, grapple) is a tuning surface.

## Notes

- It renders at half-res then upscales — that's the single biggest perf lever; if
  it ever feels heavy, the raymarch `MAX_STEPS` and half-res factor are the dials.
- This binary is intentionally a dead-end-by-design *proof*. Once the look is
  undeniable, the real engine (per `docs/ROADMAP.md`) grows from Phase 1 — and the
  shader here eventually becomes content of the shader IR, not hand-written WGSL.
- Next obvious upgrades: GPU-timestamp profiling (separate GPU ms from CPU),
  and exposing the morph/feedback/palette knobs live.
