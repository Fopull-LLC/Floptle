# Floptle — World Rules: the Lawset / Realm meta-spine (`floptle-rules`)

A game is a simulated **universe** whose developer-defined laws **compose**; this doc designs the single
object that says *which laws hold where* — a serializable, inheritable `Lawset` bound to an SDF `Realm`,
resolved by the inside-test the engine already runs.

> Decision & rationale: [`../decisions/0018-lawset-realm.md`](../decisions/0018-lawset-realm.md).
> Reads-with: the cross-field coupling graph [`./field-interaction.md`](./field-interaction.md) (ADR-0019, the
> sibling half), and the four law-axes it indexes — gravity [`./gravity-and-density.md`](./gravity-and-density.md),
> light [`./light.md`](./light.md), and time [`./time.md`](./time.md).

`floptle-rules` is a new **thin, pure-data** crate. It depends only on `floptle-core` (ids, math) and
`floptle-field` (the SDF substrate + inside-test). It is **read-only** to render, physics, and matter: those
crates *sample* the resolved laws, none of them is a dependency of `rules`, so there is no cycle. This is the
meta-spine — it holds no logic of its own beyond resolution; it indexes the models the other subsystems
already designed.

## 1. The missing primitive

Floptle's thesis is that geometry, matter, gravity, light, and time are the same kind of object — a field
sampled at `(p, t)` over one SDF substrate — so their rules **compose**. We built the substrate. We did not
build the thing that names the rules. Today they are scattered:

```
gravity   →  a per-body component / authored region   (floptle-physics)
light     →  a transport rule in the renderer          (floptle-render)
time      →  one global scalar dt                       (floptle-core)
matter    →  knobs on a material                        (floptle-matter)
```

There is **no single object meaning "the laws of this world."** That absence is the bug. Four independent
advisory lenses — light, time, world-rules, emergence — each reached for the *same* container without
coordinating: a region that carries a set of laws. The substrate was quietly built toward it. This doc makes
it literal.

## 2. Lawset and Realm

Two structs, one resolution rule.

- A **`Lawset`** is a first-class, serializable, **inheritable** struct: *the laws of a region.* Every axis
  defaults to `Inherit` (take the parent's), so a Lawset states only what it *changes*.
- A **`Realm`** binds a `Lawset` to an **SDF volume** (an `f(p) < 0` test). "Which laws hold at `p`" is just
  *which realms contain `p`* — answered by the inside-test the field crate already runs every frame.

Realms form a **tree**. A child realm whose volume sits inside a parent's **overrides parent laws
axis-by-axis**: it wins on the axes it sets, inherits the rest. Scalar laws (notably `time_rate`)
**crossfade** across the realm's `smin` boundary band — the same soft-union the CSG already uses — so a law
*changes smoothly as you cross in*, never snaps.

```
ROOT realm  (the world's default laws)
   ├─ Realm "void"        f_void(p)<0     overrides: gravity, light
   │     └─ Realm "core"  f_core(p)<0     overrides: time_rate, light   (deeper override wins)
   └─ Realm "garden"      f_garden(p)<0   overrides: matter
```

```
resolve(p) -> EffectiveLaws:
   stack = [ROOT.lawset]                       // always present
   for realm in tree, in stable order:
       if inside(realm.sdf, p):                // the test we already do
           stack.push(realm.lawset)            // deeper = higher priority
   eff = stack.fold(EffectiveLaws::root, |acc, ls| acc.override_with(ls))
                                               // axis-by-axis: Inherit keeps acc; set replaces
   crossfade scalar axes (time_rate, scale) across each contributing smin band
   return eff
```

`override_with` is per-axis: an axis set to `Inherit` leaves `acc` untouched; any concrete model replaces it.
That is the whole composition law. Order is content-independent (tree depth, then stable realm id) so the
result is deterministic regardless of iteration accidents.

## 3. The axes

The axes are exactly the law dimensions we already designed. Each is a **lean enum of 3–4 named models** plus
`Inherit` — *not* a bag of sliders. A model carries only the few parameters that model needs.

```rust
// floptle-rules — pure data, no logic beyond Default = Inherit everywhere.

pub struct Lawset {
    pub gravity:   GravityLaw,      // how "down" is defined here
    pub light:     LightLaw,        // the transport rule here
    pub time_rate: Option<f32>,     // proper-time rate r; None = Inherit, 1.0 = real-time
    pub matter:    MatterLaw,       // material/deformation regime
    pub scale:     ScaleLaw,        // space axis — DESIGNED, not wired (see §8)
}

pub enum GravityLaw {
    Inherit,
    Constant { dir: Vec3, strength: f32 },   // tier 0
    Source   { center: Vec3, strength: f32 },// tier 1, points at a body
    SdfSurface { strength: f32 },            // tier 2, down = -∇f (the headline)
    DensityField,                            // tier 3, Poisson (research)
}

pub enum LightLaw {
    Inherit,
    Conventional,                            // straight rays — the free default
    Radiance { field: FieldId },             // mood-driven shade hook
    BentTransport { bend: FieldId },         // rays curve along a field
    Media { sigma: FieldId },                // participating media (research)
}

pub enum MatterLaw { Inherit, Rigid, Soft, Soup, Fracture }
pub enum ScaleLaw  { Inherit, Uniform(f32) }
```

Maps onto the sibling tiers verbatim: `GravityLaw` is the gravity tiers (ADR-0014), `LightLaw` is the light
tiers (ADR-0016), `time_rate` is the rate field `r(p)` (ADR-0017), `MatterLaw` is the matter regimes
(ADR-0013). The Lawset does not *re-implement* any of them — it *selects* one and hands the system its
parameters.

Two properties make this cheap:

- **Absent laws cost nothing.** Every field is `Inherit` / `None` by default; an untouched axis adds zero
  bytes of meaning and zero work — resolution skips it.
- **Resolve once, cache.** A body's effective laws are computed **once per body per fixed step**, at the same
  quiet point as the floating-origin rebase and `LocalTime::advance`, then cached on the body for every system
  that reads it that step.

## 4. Anti-property-soup

The VISION explicitly rejects property soup, and this is where that discipline lives or dies. The rule:
**models are named presets, not a thousand sliders.** `SdfSurface { strength }` is one decision plus one
number, not forty checkboxes. An axis you do not touch **inherits** and never appears in your authored data.

The editor "Laws" inspector follows from this. It is a short column of **dropdowns** — one per axis — each
defaulting to `Inherit`, with a tiny parameter row appearing only when you pick a concrete model. It is **not**
a giant form. Picking `Gravity: SdfSurface` reveals a single `strength` field; everything else stays a quiet
"Inherit (parent)." You author a universe by making four or five choices, not by filling out a spreadsheet.

## 5. Identity — `lawset.ron`

A Floptle world's laws live in a `lawset.ron` you can **diff**, **hot-reload**, hand to an **AI**, and **gift**
as *"here are the laws of this universe — bend them."* That artifact is the open-source statement in code: no
incumbent ships a single readable object that *is* the rules of a world.

```ron
// lawset.ron — a root and two nested realms, each bending different axes.
Lawset(
    gravity:   Constant(dir: (0.0, -1.0, 0.0), strength: 9.8),
    light:     Conventional,
    time_rate: Some(1.0),
    matter:    Rigid,
    scale:     Inherit,

    realms: [
        Realm(
            name: "spire",
            sdf:  Sphere(center: (0.0, 20.0, 0.0), radius: 14.0),
            smin: 1.5,                       // crossfade band — scalar laws feather in
            lawset: Lawset(
                gravity:   SdfSurface(strength: 11.0),   // down = nearest fractal wall
                time_rate: Some(0.5),                    // half-speed inside
                light:     BentTransport(bend: "spire_warp"),
                // matter, scale: omitted -> Inherit
            ),
            realms: [
                Realm(
                    name: "eye",
                    sdf:  Sphere(center: (0.0, 26.0, 0.0), radius: 3.0),
                    smin: 0.8,
                    lawset: Lawset(
                        time_rate: Some(0.0),            // frozen core; deeper override wins
                        light:     Radiance(field: "haunt"),
                        // gravity inherits "spire"'s SdfSurface
                    ),
                ),
            ],
        ),
    ],
)
```

Reading `eye`: gravity = `SdfSurface` (inherited from `spire`), `time_rate` = 0 (its own override, beating
`spire`'s 0.5), light = `Radiance` (its own), matter/scale = `Rigid`/`Inherit` (from root). One file, the
whole universe, legible.

## 6. How existing systems consume it

The point of one authority is that **nobody carries a copy.** Each system reads the effective laws at a body's
position instead of owning its own rule:

```
resolve(p) once/step  ─►  EffectiveLaws (cached on body)
        ├─ physics    reads eff.gravity   -> samples g(p) per the selected tier
        ├─ render     reads eff.light     -> picks the transport rule for that body/ray
        ├─ time        reads eff.time_rate -> feeds r into LocalTime::advance
        └─ matter     reads eff.matter    -> selects the deformation regime
```

This *moves* authority, it does not add a layer: gravity stops living on a per-body component and becomes a
**per-realm default**; the renderer stops hardcoding straight rays and asks the lawset. The Lua surface is
two calls:

```lua
local eff = rules.effective_at(p)             -- table: { gravity=..., time_rate=..., light=..., matter=... }
rules.set_realm_law("spire", "time_rate", 0.25)  -- retune one axis of one realm, hot
```

Logic stays in Lua scripts; the lawset stays **declarative**. `set_realm_law` mutates data and invalidates the
per-body cache; it does not run rule code.

## 7. Relationship to the sibling

The meta-spine has two halves and this doc is one of them:

- **Lawset / Realm (this doc, ADR-0018)** says **WHICH laws hold WHERE** — selection over space.
- **Field-interaction graph ([`./field-interaction.md`](./field-interaction.md), ADR-0019)** says **how fields
  AFFECT each other** — coupling between the quantities those laws govern (density steepening gravity, gravity
  bending light, a slow region damping a morph).

A realm picks the models; the interaction graph wires the models' outputs into each other's inputs. Neither
subsumes the other. Together they make "developer-defined rules that compose" a buildable thing rather than a
slogan.

## 8. Out of scope

- **A visual rule-scripting / visual-programming language.** The Lawset is **declarative law-axes** — a fixed
  menu of named models, not a node graph you wire arbitrary logic into. Imperative behavior stays in Lua.
- **The space / portal axis.** `ScaleLaw` is sketched above so the seam is shaped, but non-uniform space,
  portals, and folded geometry are a later, large item — designed, *not wired*.
- **Per-axis runtime authoring UI beyond dropdowns.** No formula editor, no curve-per-law. Models only.

## 9. Determinism & performance

- **Pure-data, low-layer.** `floptle-rules` holds no system state and depends only on `core` + `field`, so it
  cannot form a cycle with the systems that read it. Resolution is a pure function of `(realm tree, p)`.
- **Resolution is cheap.** It rides on the **inside-test the engine already runs** — no new spatial query. The
  cost over baseline is folding a short stack of Lawsets, which is a handful of enum copies.
- **Cache per body / per step.** Resolve once at the quiet point, store `EffectiveLaws` on the body, read it
  everywhere that step. Bit-for-bit reproducible: same tree + same start-of-step `p` + stable realm order →
  same laws, every replay.
- **Crossfade is bounded.** Only scalar axes (`time_rate`, `scale`) feather across `smin`; enum axes switch at
  the boundary. A body inside *N* nested realms folds *N* lawsets — *N* is tiny by construction.

## 10. Near-term proof — the thin seam

The engine is ~100:1 docs-to-code. This lands as a **thin seam built now but wired narrow**: the `Lawset`
struct, the `resolve(p)` resolver, and per-body/step caching — with only **two axes actually wired**, the two
we can prove first:

1. **Gravity** moved off the per-body component to a **per-realm default** (`GravityLaw`).
2. **`time_rate`** driving the existing `LocalTime::advance` (ADR-0017).

Everything else — light, matter, scale, the full multi-axis editor, the interaction-graph coupling — is
**deferred**. The spine exists to keep the *seams* right cheaply; it must never big-bang against the work of
getting pixels on screen.

> **The 30-second demo.** Two adjacent `Realm` volumes sharing one `smin` band. Walk the player from a normal
> room (root: `Constant` down, `time_rate` 1.0) into a realm where **down points at the nearest fractal wall**
> (`SdfSurface`) **and** time runs at **0.5×** — *both changing smoothly across the same boundary.* One inside-
> test, one resolver, one cache; two laws bending together as you cross in. That is the entire thesis, on
> screen, in half a minute — proven small before it is asked to carry the rest of the axes.
