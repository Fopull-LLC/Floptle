# ADR-0017 — Time as a rate field

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
Space, matter, gravity, and (now) light are promoted to fields, but **time** is
still a single global scalar `t` threaded through every `f(p,t)`. The morph/carry
magic (`∂f/∂t` → surface velocity, ADR-0012) is *already* a time derivative — so
time is the natural next field, unlocking slow/freeze/dilate/echo regions for
surreal worlds and gameplay.

## Decision
Promote time to a **rate field `r(p)`** with per-entity **local clocks**:
`dτ = r(p)·dt`. The **global fixed timestep stays the authoritative master clock**
(essential for determinism + the networking goal); each warped entity carries a
`LocalTime { tau, accumulator }` draining sub-steps. A region is authored as a
`TimeRegion` (box/sphere with a rate). Time becomes a **law axis** in
`floptle-rules`; `LocalTime` lives in `floptle-core::time`.

**Determinism invariants (non-negotiable):** sample `r` **once per body per global
step** at the start-of-step position (no mid-step re-sample); fix iteration order;
forbid self-reference cycles; **hard-cap** sub-steps so a fast-time zone degrades
*rate*, never framerate or determinism. Advance is a pure function of the global
step count, evaluated at the same quiet point as the floating-origin rebase.

## Why
- Slow/fast/freeze/dilation all fall out of the *same* machinery, and it composes
  for **free** with morphing worlds: `r=0` freezes a fractal mid-swirl while it
  keeps churning a meter away (morph *is* a function of `t`).
- It's the literal temporal twin of the hierarchical reference-frame tree already
  built for space (ADR-0015).

## Alternatives considered
- **Global time only** — no surreal time play; rejected.
- **Naïve per-object timers** — break determinism and future netcode.

## Consequences
- **Ship time-SCALE regions first.** Defer time-**reverse**/echo (bounded
  ring-buffer traces of *tagged* entities only) — genuinely hard over stateful
  XPBD/plastic matter; flagged research, like the gravity Poisson tier.
- Full design: [`../subsystems/time.md`](../subsystems/time.md).
