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

**The map is a morphing, POROUS rounded MENGER SPONGE** — a real 3D fractal you go
*inside* of, not a planet you orbit. The Mandelbox is gorgeous but un-walkable
(chaotic normals, empty interior — measured); the Mandelbulb is walkable but
*solid* — "just a bumpy planet" you can only skirt the edge of. The Menger sponge
is the one that's **porous**: ~88% open, a lattice of tunnels and chambers with
solid walls you can stand on, climb, and **delve through forever inward** (measured:
~17° normal rotation/step, `|∇f|≈0.71`). **Gravity points down the local distance
gradient** (`-∇f`) toward the nearest wall, so "down" is whatever surface you're on
— you walk up the inside of a shaft and the world rolls under you. As it slowly
rotation-morphs, a time-difference surface-carry **offsets you cleanly** with the
shift.

You spawn on a **moon** with the sponge on the horizon; walk/jump off and gravity
hands you to it. A **grappling hook** (click) and a deliberately **no-air-control**
platformer (coyote + jump buffering + asymmetric jump arc + squash-and-stretch)
round it out, plus **noclip (V)** to fly around and inspect.

**Infinite descent — shrink and walk in:** *you shrink* (scale = `2^(-dive)`), so
the sponge's sub-tunnels open up around you into ever-finer chambers and each level
the sponge unfolds another iteration of detail. Because the player shrinks rather
than the field rebasing, there's **no pop** — the world just keeps getting bigger
around you, octave after octave, to the f32-precision limit. Everything — capsule
size, walk speed, gravity, jump, grapple reach, camera boom — scales with you.
Three ways to dive: **hold C** (deliberate descent — also un-sticks you from the
wall so you sink into the opening); **fly/fall through a hole** (auto-descent — dive
into a void and the world scales up so it opens into sub-tunnels you fall through,
recursively); **hold X** to ascend back out.

**Orientation:** your "up" only auto-corrects to the surface **while you're
grounded** (so you can walk up walls and around tunnels). In the **air your
orientation is your own** — gravity never snaps the camera back — steer it with
**Ctrl + mouse** (roll/pitch, wingsuit-style) or just keep whatever you had; it
re-levels to the surface when you land. The **jetpack is unlimited and strong**
(easily beats gravity) and fires an obvious **flame plume** below you whenever it's
thrusting (Space = up, WASD = directional).

```bash
cargo run -p floptle-proof --bin descent --release
```

| Input | Action |
|---|---|
| **C / X** | **descend / ascend** — shrink-and-walk-in, infinitely inward through the sponge (C also un-sticks you so you sink into holes) |
| **W A S D** | walk (grounded) / **jetpack** thrust (in air) |
| **Space** | jump on the ground; **hold in the air = jetpack up-thrust** (unlimited) |
| **Shift** | sprint |
| **Ctrl + Mouse** | **roll / pitch your whole body** in the air (wingsuit-style); air orientation never auto-snaps |
| **Mouse / click** | look; click to capture, click again to **fire + hold the grapple** (swing on the rope; release = slingshot) |
| **Scroll** | zoom; all the way in for first person |
| **V** | toggle **noclip free-fly** (Space up / Shift down) |
| **F / R** | reset camera distance / respawn |

Visuals are deliberately **stripped to clean geometry + lighting + palette** (no
feedback trails / chromatic aberration / noise) so the raw fractal is legible; the
tunnel walls and corners are tinted by cell coordinate + face + descent depth so
each octave reads as its own chromatic stratum.

> Experimental cut. Feel (descend rate, gravity-toward-walls, shrink limit, jump,
> grapple) is a live tuning surface.

## Notes

- It renders at half-res then upscales — that's the single biggest perf lever; if
  it ever feels heavy, the raymarch `MAX_STEPS` and half-res factor are the dials.
- This binary is intentionally a dead-end-by-design *proof*. Once the look is
  undeniable, the real engine (per `docs/ROADMAP.md`) grows from Phase 1 — and the
  shader here eventually becomes content of the shader IR, not hand-written WGSL.
- Next obvious upgrades: GPU-timestamp profiling (separate GPU ms from CPU),
  and exposing the morph/feedback/palette knobs live.
