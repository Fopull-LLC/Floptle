# Accessibility

Four settings, one Lua table, and the engine honours the parts it owns.

```lua
-- an options menu, in full
function start(node)
  access.setTextScale(save.get("textScale") or 1.0)
  access.setColorFilter(save.get("colorFilter") or "none")
  access.setReducedMotion(save.get("reducedMotion") or false)
  access.setCaptions(save.get("captions") or false)
end
```

These are the **player's** settings, so they belong in the player's save — read
them back with `access.*` and store them with `save.set`. The editor's
**⚙ Settings → ♿ Accessibility** drives the same values, so you can try your game
with each one on without writing a script first.

> Until this existed, the engine's entire accessibility surface was input
> rebinding — and that exists by accident of the action-map work rather than by
> intent. Console platform holders require a subset of this, and roughly 1 in 12
> men has some colour vision deficiency. An engine that calls it "the game's
> problem" pushes it onto every game separately, and most will skip it.

## Text scale

```lua
access.setTextScale(1.5)     -- 0.5 – 3.0
local s = access.textScale()
```

Multiplies **every UI text size**, applied before the layout solver runs. That
ordering is the whole feature: the solver measures the *scaled* run, so a
`height = "fit"` box grows and everything below it moves down. Scaling at draw
time instead would paint bigger glyphs into the same rect — i.e. clip, at exactly
the sizes somebody turned the setting up to reach.

**What you should do to be ready for it:** prefer `height = "fit"` and `wrap` on
anything holding text. A box with a fixed height and `overflow = "clip"` will
still clip at 3×, because you told it to. `textFit` text is left alone
deliberately — its size already comes from the box it fills.

Out-of-range values **raise**. A settings slider hands over a number it already
bounded, so anything outside 0.5–3 means the caller computed it wrong, and a
silently clamped `0.1` is a slider that appears to stop working.

## Colour vision

```lua
access.setColorFilter("deuteranopia", 0.9)   -- name, optional strength 0–1
for _, f in ipairs(access.filters()) do      -- build a dropdown from the engine
  print(f.name, f.label)                     -- "deuteranopia", "deuteranopia (green-blind)"
end
```

| name | who | share of men |
|---|---|---|
| `protanopia` | red-blind | ~1% |
| `deuteranopia` | green-blind | ~6% |
| `tritanopia` | blue-blind | ~0.01% |

The filter is a stage in the post chain, so it applies to **everything the player
sees** — the 3D scene, the HUD, the lot. It deliberately survives a scene whose
`PostProcess` node is disabled: a scene must not be able to veto a setting the
player turned on.

It **corrects** rather than simulates. The picture is converted to cone
responses, the missing cone's axis is collapsed to find what that viewer loses,
and the lost difference is pushed onto the channels they *can* still separate. So
red-and-green, which arrive as one olive, come back as pink-and-green.

`strength` exists because a full correction shifts hues a long way. Some players
want the separation without the shift.

**Simulating, for you.** Tick *Simulate instead* in ⚙ Settings to see what a
player with that deficiency sees, rather than the corrected picture. That is a
developer's check — a way to find the red-on-green "press to continue" prompt
before somebody else does — and not something to ship switched on.

**Do both halves.** A filter is a rescue, not a design. Never carry meaning on
hue alone: put a shape, an icon, a position or a label alongside the colour. Red
and green fuel bars that differ only in colour are unreadable to 1 in 12 men with
the filter off, and merely *different* with it on.

## Reduced motion

```lua
if not access.reducedMotion() then
  shakeCamera(0.4)
end
```

The engine snaps its own UI transitions when this is on — a hover still
*changes*, it just does not slide. Snapping rather than hurrying is the point: a
40 ms slide is still a slide, and vestibular triggers are about movement existing
at all.

**The engine cannot do this for you.** It has no camera shake of its own, so
yours has to read the flag. Same for screen flashes, big animated wipes, and
anything that moves the whole view. `tween` is deliberately *not* gated — most
tweens are gameplay, and silently freezing them would break games.

## Captions

```lua
caption("a door unlocks somewhere below")     -- duration suits the line
caption("INCOMING", 2.0)                      -- or say how long
```

Drawn by the engine: bottom-centre, on a dark plate, at the player's text scale,
oldest first, at most four at once. Every game gets the same readable placement
instead of hand-rolling one — and a game that hand-rolls it gets it subtly wrong
(too high, too small, behind the HUD), which the players who need captions are the
last to be asked about.

While `access.captions()` is off, `caption(...)` draws nothing and returns
`false`. That is deliberate, so you write `caption(...)` beside the sound and
never an `if` around it.

## What is deliberately not here

**A screen-reader path.** It is a much larger piece of work — a semantic tree,
focus order, a platform bridge — and it wants its own card rather than a token
gesture inside this one. What exists today that helps: every UI element already
has a `name`, and keyboard navigation already works through `navUp`/`navDown`/
`navLeft`/`navRight` plus the focus hooks.

**Colourblind-safe default palettes.** The engine imposes no look — it never has
— so it does not ship a palette to override. The filter plus the advice above is
the honest version of that half.
