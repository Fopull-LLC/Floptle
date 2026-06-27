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

## Notes

- It renders at half-res then upscales — that's the single biggest perf lever; if
  it ever feels heavy, the raymarch `MAX_STEPS` and half-res factor are the dials.
- This binary is intentionally a dead-end-by-design *proof*. Once the look is
  undeniable, the real engine (per `docs/ROADMAP.md`) grows from Phase 1 — and the
  shader here eventually becomes content of the shader IR, not hand-written WGSL.
- Next obvious upgrades: GPU-timestamp profiling (separate GPU ms from CPU),
  and exposing the morph/feedback/palette knobs live.
