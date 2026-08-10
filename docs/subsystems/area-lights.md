# Area lights — lights with a size

*Status: shipped in v0.47.0. Diffuse direction is analytic; the terminator wrap
and the specular representative point are fitted approximations, and this page
says which is which.*

A point light is a mathematical convenience. Nothing in the world is one — a
window is two metres of glass, a strip light is a metre of tube, a bulb has a
bulb. The difference shows up in three places at once: the shape of a highlight,
the softness of a terminator, and the direction light arrives from when the
emitter is close and wide.

Every placeable light now carries the surface it emits from. **`Point` is the
default and the zero-size case**, and the whole implementation collapses to
`max(dot(n,l),0) × (1 - d/range)²` when the emitter has no size — the expression
that was inline before, reached numerically and not approximately.

---

## The shapes

| shape | what it is | oriented by |
| --- | --- | --- |
| **point** | a dimensionless source | — |
| **sphere** | a bulb with size | — |
| **rect** | a window, a softbox, a screen | faces the node's **forward** (-Z) |
| **disk** | a downlight, a porthole | faces the node's **forward** |
| **tube** | a strip light, a neon bar, a blade | lies along the node's local **X** |

A rect and a disk are **one-sided** by default — a window lights the room, not
the wall it is set into. Turning *lights both ways* on makes it a panel that
glows from both faces.

The node's **scale** multiplies the emitter, so dragging a scale handle on a
window does what it looks like it does.

## Diffuse: the part that is exact

The vector irradiance of a polygon,

```
w = (1/2π) Σᵢ θᵢ ûᵢ
```

(θᵢ is the angle edge *i* subtends at the shading point, ûᵢ the unit normal of
the wedge it sweeps) is **linear in the surface normal**. That is the useful
fact: one loop over the edges gives a single vector, and the emitter's own
lighting direction is `ŵ`. It is not the direction of the emitter's centre —
for a four-metre strip standing beside a wall those differ by a lot, and that
difference is exactly why the wall lights evenly instead of showing a hot spot
opposite the middle.

`area_light_probe` checks this against quadrature over the emitter's real
surface, at three different surface normals, and it agrees to within 8-bit
quantisation.

**The terminator softening is a fit.** Once the direction is known, the response
is `clamp((n·ŵ + s) / (1 + s), 0, 1)` where `s` is the emitter's apparent angular
half-size. It is the right shape — a big light wraps past the horizon, a small
one does not, and `s = 0` is exactly `max(n·ŵ, 0)` — but it is not the clipped
polygon integral, and a very large emitter very close to a surface will be a few
percent off.

**Range falloff measures to the emitter's nearest point**, not its centre, and
that measurement is view-independent. A three-metre bar whose centre is out of
range still has an end beside you; a light that dimmed as you walked around it
without moving would be unplaceable.

## Specular: the representative point

For the highlight, each shape reports the point on itself nearest the mirror
direction, and the lobe is widened by the emitter's apparent size and
re-normalised so growing a light **spreads** its highlight rather than adding
energy to it.

- a **sphere** gives the classic disc highlight
- a **rect** gives a broad soft rectangle, clipped to its own extents
- a **tube** streaks along its own length — measured in the probe as a highlight
  more than twice as wide as it is tall, against a point light's round one

This is an approximation and a well-known one. It is not LTC: there are no
fitted lookup tables, the energy is not exact, and a rect seen at a grazing angle
does not horizon-clip its highlight properly. What it buys is that every shading
path in the engine gets it — the raster PBR path, the Blinn-Phong path, the
raymarched terrain and blobs and `.flsl` materials — from one function, with no
tables to ship and nothing to bind.

## In the fog

Volumetric fog reads the emitter too, but takes only its **distance and
direction** — there is no surface in mid-air to be facing anything, so there is
no `N·L` term to replace. The visible consequence is that a long bar lights the
air along its whole length rather than from a point at its middle. See
[`./volumetrics.md`](./volumetrics.md).

## Editing it

The Inspector's light node: pick **emits from**, then its dimensions. Switching
shape carries the size across, so trying rect against disk is one click rather
than a re-measure.

The Scene view draws the emitter at its real size and facing, with an arrow out
of the emitting face. That matters more than it sounds: **a rect light aimed at
the wall behind it lights nothing, and there is no way to see that in the
finished picture** — the room is simply dark and the light looks like it is on.

From Lua, on `node:getcomponent("PointLight")`: `shape` (0 point, 1 sphere,
2 rect, 3 disk, 4 tube), plus `width`, `height`, `radius`, `length`,
`thickness`, `twoSided`. A dimension reads 0 on a shape that has no such
dimension, and writing one only lands on a shape that has it. Assigning `shape`
keeps the size the emitter had, so cross-fading a window into a bulb does not
flash.

## Cost

Per light per fragment: a point costs what it always did. A sphere or disk adds a
handful of arithmetic. A rect adds four `acos` calls and four cross products; a
tube adds one segment projection. All sixteen slots are shared with point lights
and the same *contribution* ranking chooses them, so an area light is not a
separate budget.

## Verified by

`cargo run -p floptle-render --example area_light_probe -- <dir>` — six checks:

1. a zero-size emitter reproduces the point light's closed form
2. a four-metre rect lights the far end of a wall a point at its centre leaves
   dark (measured as evenness, 0.95 against 0.62)
3. the analytic direction matches quadrature over the emitter's surface, at
   three different surface normals
4. a one-sided emitter does not light what is behind it — **and both the
   two-sided version and the same emitter turned around do**, so the facing test
   is not simply rejecting everything
5. a three-metre sphere reaches around onto a face a pinpoint leaves dark
6. a bar streaks its highlight along its own length and only along it

Runs under lavapipe in CI.

## Not in this one

- **LTC.** Linearly-transformed cosines would make the specular energy-exact and
  horizon-correct, at the cost of two fitted lookup textures. The seam is one
  function (`area_terms`), so it can be swapped without anything above it
  changing.
- **Shadows shaped by the emitter.** An area light still casts through the same
  sun/field shadow machinery as everything else; its softness comes from the
  penumbra setting, not from its own width.
- **Textured emitters.** A rect light emits one colour, not an image.
- **Emitter geometry.** The light does not draw itself — put a mesh with an
  emissive material where the emitter is if you want to see it in the frame.
