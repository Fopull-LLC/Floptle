# Volumetrics — light in the air

*Status: shipped in v0.46.0 (single-scattering fog with light injection). The
media is one global height layer; per-node fog volumes and multiple-scattering
are recorded at the bottom as follow-ups.*

Fog used to be a colour the picture faded toward. Distance ramp or marched
layer, it did not matter — the value being mixed in was a constant, so the sun
standing directly behind a fog bank made no difference to it, and neither did a
lamp sitting inside one.

**Light injection** makes the media take the scene's light. At every step of the
per-pixel march the fog now asks what light reaches *that point*, and that is
what it scatters toward the camera. A shadow crossing the air stays in the air —
which is where beams through windows and branches come from, and they are not a
separate effect but a consequence of asking the question at all.

---

## The composite, and why the old look is reachable exactly

Single scattering, marched front to back:

```
scattered += T · (1 - e^(-σ·dt)) · L(p)
T         *= e^(-σ·dt)
final      = behind · T + scattered
```

`σ` is the media density at that point, `T` is how much of what is behind still
gets through, and `L(p)` is the radiance scattering toward the camera from `p`.

The reason it is written this way rather than as a blend factor: **when `L` is a
constant `C`, the sum telescopes to `C·(1 - T)` and the whole composite collapses
to `mix(behind, C, 1 - T)`** — the exact expression the flat volumetric fog used.
So the *lit by the scene* amount at 0 is not an approximation of the old
appearance, it is the old appearance, independent of the step count and of the
per-pixel jitter. `fog_probe` checks that against closed-form arithmetic rather
than against a control frame, because a control frame rendered by the same
rewritten code would agree with itself no matter what it did.

Above 0 the fog colour stops being what the media *looks like* and becomes what
it is *made of* — its albedo, multiplying the light that arrives:

```
L(p) = fog_color · mix(1, inscatter(p) · gain, amount)
```

so warm fog under a blue sun is both, and neither one alone.

## What `inscatter` counts

| term | cost | note |
| --- | --- | --- |
| the sun / every star | one shadow march per step, when **shafts** is on | this is the beam |
| every point light | a distance and a phase | why a torch has a visible cone |
| the baked bounce | **one** probe fetch per ray, not per step | see below |
| the flat ambient | free | the floor when everything else is occluded |

The bounce is sampled once, at the middle of the marched span. A probe fetch is
32 texture loads; per step it would cost more than the shadow march it sits next
to, and the bounce varies far more slowly along a ray than the media does. The
consequence is worth stating plainly: fog gets *the room's* bounce, not a
per-point one.

## The phase function

A mote of fog has no facing, so there is no `N·L` to lean on. What replaces it is
the **phase function** — which direction the media throws the light it receives.
Floptle uses Henyey-Greenstein, normalised so that isotropic reads 1.0 rather
than the physical 1/4π, because every other lighting knob in the engine is in
"a surface facing the light reads 1" units and a phase arriving at 0.08 would
make the amount slider mean something different from all of them.

```
phase(cosθ, g) = (1 - g²) / (1 + g² - 2g·cosθ)^1.5
```

Positive `g` scatters forward: look toward the sun through the layer and the air
blooms, look away and it stays dim. That asymmetry is most of why lit fog reads
as *atmosphere* rather than as *a brighter wash*. Negative `g` throws it back at
you, which is closer to what thick cloud does.

## The sky

Volumetric fog composites over sky rays. The depth ramp deliberately does not —
it is a stylistic distance ramp, not a medium, and fogging the sky with it is a
flat wash over a skybox that should read crisp.

A fog **layer** is different: it is bounded in height, and a ray leaving the
world really does pass through it. Leaving it out is what put a hard seam at the
horizon (hence the old advice to match the fog colour to the sky) and what hid
every shaft that had sky behind it rather than geometry.

An upward ray exits the layer at a height the shader can solve for, so most sky
pixels march a fraction of the fence. The `max distance` knob is that fence, for
the rays that never exit.

## Cost

The march is `steps` samples per pixel, and with **shafts** on each sample is a
full sun-shadow march — the same one a surface pays for once. It is the most
expensive thing in the fog by a wide margin, and it is also the entire beam, so
it is a checkbox rather than a hidden cost.

Three ways down, in the order worth trying:

1. **Shafts off.** Lit fog with no occlusion: the sun still colours the air, and
   nothing carves it. Costs about what the flat fog cost.
2. **Fewer steps.** The step count does not change the *brightness* (see the
   telescoping sum above), only how smoothly the media resolves, so it is a
   genuine quality dial and not a look dial.
3. **`fog_light = 0`.** The flat layer, exactly as it was.

The march is bounded by the density: a step in air thin enough to contribute
nothing skips its shadow march entirely, so a layer that sits below the camera
costs almost nothing for the sky above it. A ray also stops once its
transmittance falls below a thousandth — nothing behind fog that thick is going
to be seen, and dense fog is the case that marches most.

### What it used to cost, and why (v0.54.0)

The first scene the GPU profiler was pointed at spent **six of its eight
milliseconds here**, on four ceiling lights in a corridor. Three separate
mistakes, all in the per-step loop, all now fixed — worth writing down because
each is the kind that looks correct while reading it:

- **`fog_inscatter` asked every lamp for a full surface-lighting calculation and
  used two numbers out of it.** `area_terms` integrates an emitter's solid angle
  against a surface normal and finds a representative point for the highlight; a
  rect emitter costs four quaternion rotations and an edge loop to do it. **There
  is no surface in mid-air** — no `ndl`, no mirror direction — and the code even
  said so in a comment while calling it anyway. `fog_emitter` is a length and one
  subtraction, and the subtraction (the emitter's own size, via `fog_extent`) is
  the only part that mattered: the air beside a strip light should be lit by the
  strip, not by a point in the middle of it.
- **The range test came after the expensive part.** A lamp at the far end of a
  level was evaluated in full and then multiplied by zero. It is now a squared
  distance against the range widened by `fog_extent`, so it can only ever reject
  a lamp the evaluation would have rejected too.
- **The noise was sampled about forty times per lump.** It is tens of metres
  across; a step is a fraction of a metre. `fog_noise_stride` holds one sample
  for a run of steps sized so the field is still sampled eight times across its
  finest feature, and `cloud_fbm_lod` stops evaluating octaves finer than a step
  can resolve — replacing each with its **mean**, not with nothing, because
  truncating an octave removes its average too and the fog then visibly thins
  with distance.

Alongside them, `fog_inscatter` now returns early on the **sun** when
`light_color` is black, which is every interior. Note the shape of the bug that
came out of writing that: it was first an early `return` from the whole function,
which also skipped the placeable lamps below it — so a room lit entirely by its
own ceiling panels got no light in its air at all. `fog_probe` caught it.

Together: **6.0 ms → 0.9 ms**, with the frames pixel-identical.

## Editing it

The Lighting node, under **fog → volumetric**:

- **lit by the scene** — 0 is the flat colour, 1 is the media lit by the sun, the
  point lights and the baked bounce, past 1 exaggerates.
- **forward scatter** — the phase `g`.
- **shafts** — march the sun shadow per step.
- **quality** — steps per ray.
- **max distance** — the fence for a ray that hits nothing.

From Lua, on the Lighting node's `Light` component: `fogLight`,
`fogAnisotropy`, `fogSteps`, `fogShafts`, alongside the existing `fogDensity`,
`fogHeight`, `fogFalloff`, `fogNoise`, `fogNoiseScale`.

## Verified by

`cargo run -p floptle-render --example fog_probe -- <dir>` — five checks, four of
them control pairs and one against arithmetic:

1. the amount at 0 lands on the closed form for flat fog, at 4, 8, 16 and 48 steps
2. raising the amount brightens the air, and a blue sun makes warm fog blue
3. the measured forward/backward ratio matches Henyey-Greenstein at that `g`, and
   at `g = 0` the sun's position stops mattering at all
4. an occluder darkens the air under it — **and with shafts off the same
   occluder changes nothing**, which is the assertion that separates the fog's
   own shadow march from anything else that might have dimmed it
5. a lamp glows on its own side of the frame, in its own colour, and stops doing
   so at amount 0

**`fog_probe` runs with the noise switched off**, deliberately — its arithmetic
checks want a density that is exactly constant. That also means it covers none of
the noise work above, and every assertion in it passed unchanged while that code
was being rewritten underneath.

`cargo run -p floptle-render --example fog_noise_probe -- <dir>` is the one that
does. Its assertions are **convergence rather than appearance**, which is how to
ask whether a held sample landed where the un-held march was going without a
golden image to compare against:

1. switching the noise on moves the picture, measured pixel against matching
   pixel — a frame's own spread is dominated by the lamp's glow and would pass
   with the noise off entirely
2. the same fog marched at 8, 16 and 48 steps — three different strides over one
   field — comes out the same brightness (measured: within 0.75%)
3. fog whose noise is too fine to resolve is still as **thick** as coarse fog,
   which is the check on replacing a dropped octave with its mean

Both run under lavapipe in CI.

## Not in this one

- **Per-node fog volumes.** One global layer, scene-wide. A box of fog you can
  place in a room is the obvious next shape and the marcher is already
  positional; what it needs is bounds to intersect, not new lighting.
- **Multiple scattering.** Real thick fog glows *around* a light rather than only
  along the ray to it. Single scattering plus a forward phase gets most of the
  look; the rest is a second, much heavier tier.
- **Media in the shadow march.** Fog does not currently shadow itself or dim what
  is behind it *for other lights* — only the camera ray integrates it.
- **Particles in lit fog.** Particles still take the depth-ramp fade rather than
  the marched media.

See also [`./light.md`](./light.md) §5, which specified participating media as a
research tier — this is the unsigned, always-finite half of it;
[`./shadows.md`](./shadows.md) for the march the shafts reuse; and
[`./global-illumination.md`](./global-illumination.md) for the bounce the media
picks up.
