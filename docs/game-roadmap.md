# Floptle Solar — Game Roadmap: the space-company game

*2026-07-21 (rev 2, after Ty's full-vision brief). Companion to
`solar-demo-plan.md` (superseded as the forward plan — S0–S6 shipped, S7–S9
fold in below), `engine-roadmap.md`, `galaxy-streaming-proposal.md`, and
`netcode-design.md`. This is the plan of record for the game.*

## 1. The game in one paragraph

You run a small spacefaring company from a home base on an Earth-like planet
you name yourself, inside a **procedurally infinite galaxy** — escape your
solar system, cross deep space, and there is always another system out there,
but it's *real* positioning all the way down: with the right navigational
gear you can always find your way home. You assemble ships in a builder that
feels like snapping Lego together in a 3D modeling tool, fly honest physics,
and work the destinations: mine, dig, extract water and fuel, harvest flora,
hunt (or protect) alien fauna, research the unknown, erect bases and
stations, and haul or transmit your findings home. Money buys parts;
**discoveries unlock them**; manufacturing frees you from the catalogue;
remote shipyards free you from home. Around all of it sits a living company
sim — reputation, media, lawsuits, competitors, and a stock market that
watches everything you do. It's wondrous and a little grimy: the game shows
what conquering space actually costs, and then lets you decide who you are
about it. Sandbox freedom is sacred — the game supplies consequences, never
walls.

**The loop:** BUILD → EXPEDITION (fly / land / dig / research / harvest) →
RETURN or TRANSMIT (sell, bank discoveries) → UNLOCK (parts, machines,
capabilities) → repeat — farther out, deeper down, bigger footprint — until
the company outgrows its planet, then its system, then its neighbors'.

## 2. The prime directive: we're making a game IN the engine

**We are not making a game engine to make this game.** Every capability this
game needs lands as a *generic, publicly surfaced, documented engine feature*
that any developer could use for a completely different game — or it lands as
Lua/content inside `solar/`. Nothing in the engine may know what a "uranium",
"stock market", or "astronaut" is. Floptle Solar is the proof-of-complexity,
not a special case: someone else's game should be able to be equally complex
and look nothing like it.

The ledger below is the contract. When a phase says `[engine]`, this is what
it actually adds — and each entry must make sense on its own line in the
public feature list:

| Game need | Generic engine feature it lands as | Also enables (other genres) |
|---|---|---|
| Ships from parts, decouplers, breakage | **Compound rigidbodies**: multi-shape bodies, composed mass/CoM/inertia, runtime split/merge | vehicles, destructible structures, machinery |
| Stress destruction | **Per-link load reporting** on compounds + break thresholds | bridges, cranes, wrecking games |
| Parachutes, fairings, flight | **Aero force model** (density × v² drag per shape, deployable drag/lift states) | flight sims, falling games, sails |
| Wheels, rovers, cave crawlers | **Raycast vehicle component** (suspension, drive, friction curves, surface-normal gravity opt-in) | racing, trucks, wall-crawlers |
| Trees/rocks/structures on planets | **Scatter & instancing system** (density fields, deterministic per-chunk placement, GPU instancing, LOD fade) | forests, cities, debris fields |
| Buried labs you dig into | **Terrain stamp API** (CSG carve/fit of prefabs into fields at gen or runtime) | dungeons, bunkers, ruins in any voxel game |
| Dig yields | **Sculpt yield reporting** (volume removed per palette slot) | any mining/terraforming game |
| Animated astronauts, aliens | **GPU vertex skinning** (finishes the shipped anim system) | every game with characters |
| Walkable moving ships/stations | **Carrier frames**: character/physics simulation local to a moving body | trains, elevators, capital ships, ferries |
| Breathable bases, gas, pressure | **Volume enclosure detection + per-room scalar channels** (a generic "gas/fluid state per sealed volume" primitive) | submarines, flooding, smoke spread |
| Infinite galaxy, deep-space travel | **Hierarchical large-world coordinates + seeded streaming regions** (extends floating origin; see `galaxy-streaming-proposal.md`) | any open-universe or huge open-world game |
| Alien fauna | **Population streaming** (spawn budgets/density rules near players, despawn far, deterministic from seed) + existing physics/Lua AI | wildlife, crowds, traffic |
| Media, stocks, competitors, drugs, missions | **Pure Lua + save.* + UI system** — zero engine work, and that's the point | (the systems showcase) |
| Friend visits (Terraria-style) | **Netcode phases 2a–2f as designed** + world-region streaming over the wire | co-op in any Floptle game |

If an item can't be phrased generically, it belongs in `solar/` as Lua.

### The engine release train

Engine features born here ship publicly **as engine releases as soon as
they're tested and usable** — not hoarded for the game's sake. Working
policy: a `[engine]` feature lands behind the game's needs, gets probe/test
coverage + docs + (where it fits) an example, then rides the next `v0.x`
tag with release notes written *generically* (what any dev gets, not what
Solar needed it for). Rough mapping — real cuts decided per readiness, and
patch releases keep flowing between these for fixes:

| Release theme | Features (from the ledger) | Born in |
|---|---|---|
| v0.2 "Assemblies" | compound rigidbodies + link loads + runtime split; aero forces | SC1–SC2 |
| v0.3 "Worlds" | scatter & instancing; terrain stamps; sculpt yields; biome gen hooks | SC3–SC4 |
| v0.4 "Characters" | GPU vertex skinning; population streaming groundwork | SC5 |
| v0.5 "Habitats" | enclosure volumes + room channels; raycast vehicles | SC8–SC9 |
| v0.6 "Frames" | carrier frames (walkable moving bodies) | SC11 |
| v0.7 "Galaxies" | hierarchical coordinates + seeded region streaming | SC12 |
| v0.8+ | netcode milestones per `netcode-design.md` | SC18 |

Every release is also marketing for the engine (task ledger → agent W gets
"what's new" material as each lands).

## 3. Where we stand + the asset inventory

Shipped and load-bearing: Kepler rails + patched conics + time-warp, a
pilotable ship (fuel/legs/wrecks), trajectory map, terrain 2.0 (32-slot
weight splat, digging), prefabs + deep Lua (scheduler, handles, spawn,
save.*, layers, raycasts, noderefs), physics (body modes, triggers, gravity
volumes, events), particles (DAW timeline), audio (spatial + mixer), shader
graph + .flsl stages (materials, sky, FieldShapes), vertex/texture paint, UI
system phases 1–2 + buttons/sliders/onClick, persistence.

**Asset inventory (all imported under `solar/`, licenses alongside):**

| Directory | Pack | Use |
|---|---|---|
| `models/buildings/` | Kenney City Kit Industrial (25, CC0) | home-base facility exteriors |
| `models/space-kit/` | Kenney Space Kit (153, CC0) | rocket parts, corridors, rover, craters, terrain props — **SC1 part meshes start here** |
| `models/station/` + `models/station-modular/` | Space Station Kit (97) + Modular Space Kit (40, CC0) | station modules + walkable interiors |
| `models/factory/` | Factory Kit (143, CC0) | refineries, manufacturing lines, conveyors, machines |
| `models/prototype/` | Prototype Kit (145, CC0) | scaffolding/struts/greybox parts, builder placeholders |
| `models/blasters/` | Blaster Kit (40, CC0) | handheld tools — mining laser bodies, future implements |
| `models/food/` | Food Kit (200, CC0) | agriculture produce, canteen, supply flavor |
| `models/characters/` | Animated Characters Retro (CC0) | `character_retro.glb` — rigged + **idle/jump/run clips**, embedded skin, 4 swap skins (incl. zombies…) — **the astronaut/NPC base** |
| `models/astronaut/` | Bellanger humans (CC-BY-SA) | secondary rigged humans (no clips) — NPC/civilian variety |
| `audio/kenney/{sci-fi,impact,ui,interface}/` | 354 CC0 oggs | engines/alarms/lasers, destruction, menus |
| `textures/patterns/{solid,lines}/` | Pattern packs (114 tiles, CC0) | UI, decals, .flsl material inputs |

Kits ship with a shared `Textures/colormap.png` per pack (glTF import
resolves relative URIs — keep the folders intact). Space Kit GLBs are fully
self-contained.

## 4. Design tenets (these outrank any phase detail)

1. **Builder freedom.** No part is special. Move any part or subtree freely;
   grab the whole ship; a ship is just the connected graph. **The builder
   feels like a 3D modeling package where the pieces snap like Lego** — you
   think about the craft, never about the camera or the controls.
2. **Balanced parts, honest physics.** Part *stats* serve progression pacing
   — but the flight model and instruments are real: thrust, mass, TWR, Δv,
   CoM vs. CoT, stress loads — computed with the actual math, shown
   truthfully, and consequential. Only the catalogue is a game.
3. **Progression = discovery.** Money is throughput; discoveries are gates.
   Locked parts state requirements in plain words ("Discover uranium").
4. **Everything's a toggle.** Any rule that shapes playstyle is a
   save-creation option (§10). Sandbox preset = everything free.
5. **The astronaut is a person.** Inventory with weight, health, oxygen,
   visible gear, first-person everywhere — including *inside* moving ships
   and stations. Losing one hurts (reputation, media, lawsuits).
6. **Freedom with consequences, never walls.** Bomb a competitor, strip-mine
   a biosphere, run sweatshop expeditions on synthesized stimulants — the
   game responds with markets, media, law, and the occasional horror, but it
   never says "you can't". Playing it straight must be equally viable and
   rewarding (sponsorships, trust, better prices).
7. **Surreal-vibrant with quiet dread, and gritty where it's true.** Space
   is gorgeous AND industrial, dangerous, morally complicated. Scale,
   silence, and the unexplained do the horror — subtly.
8. **Show off the shader system.** Anything that deserves unique visuals
   gets a hand-written `.flsl`: engine plumes, mining beams, shields,
   atmospheres, alien flora, warp-space, CRT terminals. The game doubles as
   the shader system's portfolio (§9).
9. **Performance is a feature.** Galaxy scale earns nothing if it stutters;
   every phase ends within frame budget on Ty's box, probes prove it.
10. **UX bar (standing):** nothing moves on its own, stable popups, wheel
    zoom / middle pan / box select + multiselect in every canvas-like tool
    from its FIRST cut.

## 5. The economy spine (shared vocabulary)

- **Materials** registry: id, tier, weight/unit, base value, rarity class,
  where-found. Terrain palette slots map to material ids per planet; digging
  yields volume × density. **Everything harvestable is a material** —
  ores, ices, water, fuels, wood, fibers, alien biomass, creature products.
- **Processing chains:** raw → refined → components → parts, via base
  machines (refinery, chem plant, manufactory). Fuel can be mined/refined;
  water extracted; **stimulants synthesized** (astronaut performance
  boosters with costs — §Arc III). Chains are data (`recipes.lua`).
- **Money.** One currency. Start = one minimal ship, nothing spare.
- **Catalogue & warehouse.** Parts: cost, mass, tier, unlock requirement.
  Bought (or **manufactured**) parts stock a facility's warehouse; building
  consumes stock.
- **Discoveries & milestones.** First-time events grant research milestones;
  transmitting data from afar yields discoveries at a discount vs. hauling
  samples home.
- **Reputation, media, law, market.** One "public company" state (Arc IV):
  reputation stars, media sentiment, pending lawsuits, stock price — all fed
  by observable in-game events, all mattering through prices, sponsorships,
  and contracts. Never through "no".
- **Save setup** (`company.ron` via `save.*`): company name, home planet
  name, toggle sheet (§10), preset (Career / Sandbox / Custom).

## 6. Arc I — The Company *(SC1–SC10: from first rocket to a working world)*

Each phase ends playable. `[engine]` = generic feature per §2 ledger.

### SC1 — Ship Builder v1 *(the centerpiece — get the feel right)*
The builder is a dedicated interior scene at the Assembly Building.
- **Part graph, no root.** Blueprint = `{ parts: [{id, part, pos, rot}],
  links: [{a, node_a, b, node_b}] }` (`.ship.ron`). Any connected subgraph
  is a ship; floating islands highlight amber and don't launch.
- **Camera: free-fly** *(Ty's call — KSP's pole-locked orbit is the enemy)*:
  WASD+RMB fly, wheel dolly, F focuses selection and orbits *it* until you
  fly again, Home reframes the ship. Grid floor + soft horizon so you always
  know which way is down; no gimbal flips, no vertical-axis jail. You think
  about the craft, not the camera.
- **Placement = Lego-snapping in a modeling tool:** pick from catalogue →
  ghost follows cursor → **attach nodes glow and magnetically snap**
  (strength falls off with distance, Tab cycles candidate nodes, hold Alt
  for free surface-attach) → click places. Drag any part = move its subtree;
  **G = grab whole ship**; R/T rotate in sane 15° steps (Shift = fine,
  within attachment-legal cones). Box-multiselect, del/dup, full undo/redo.
  Misplacing a part must never cost more than one click to fix.
- **Symmetry tools:** radial ×2–×8 around the attach target; mirror mode
  for planes/rovers. Symmetry groups stay linked afterward (edit one, all
  follow; breakable).
- **Scaffolding & structure parts** (Prototype Kit meshes): struts, trusses,
  frames, adapters — cheap inert `structural` parts so builds can get big
  and weird early.
- **Engineering gizmos (live, honest §4.2):** CoM marker, per-stage CoT
  vector, torque-misalignment warning (shows the induced arc), corner
  readout: mass, cost, per-stage Δv (real Tsiolkovsky), TWR vs. home
  gravity. Stress preview under a chosen TWR highlights weak links.
- **Staging strip:** vertical, KSP-familiar, bottom-right; auto-filing,
  free drag-reorder that never touches the ship itself.
- **Structure data:** every part carries **strength** + **material class**
  (`tank`/`engine`/`structural`/`crewed`/`canvas`/…); links carry the weaker
  face's strength. SC2 destruction reads this.
- **Starter parts** (Space Kit meshes + .flsl materials): Pod Mk1 (crew 1,
  hatch, **interior shell** — SC11 walks it), FT-S/FT-M tanks, small/medium
  engines, stack decoupler, parachute, struts, fixed legs, ladder.
- `[engine]` **Compound rigidbodies** (§2): N shapes, composed mass/CoM/
  inertia, runtime split; per-shape contact attribution.
- **Launch:** blueprint → compound on the pad; warehouse debited; existing
  ship controller reads the graph (engines = thrust points, tanks = staged
  fuel pool).
**Playable proof:** build a 10-part rocket three ways without once fighting
the camera; move the whole ship mid-build; launch; stage; land on legs.

### SC2 — First mission, honest failure, and the money loop
- `[engine]` **Aero forces:** density × v² drag per shape; **parachute** =
  staged drag spike (deploy animation, jolt, shreds above threshold);
  **fairings** = drag-shielding shells that jettison (Space Kit cones).
- `[engine]` **Stress & destruction v1:** rigid weld + analytic per-link
  stress each tick (thrust/g-load × outboard mass, aero pressure, impact
  impulses per shape). Over-limit link → **clean separation** (same split as
  decoupling). Part failure by material class: `tank`/`engine` explode —
  blast impulse + VFX + **terrain crater via sculpt API** scaled by
  remaining fuel; `structural` shatters (debris, no crater); `crewed`
  crumples (survivability window); `canvas` shreds. Failures cascade
  honestly. Craters are real dig holes — wreck sites feed SC4 salvage.
- **Launch clamps/scaffold towers** (release at ignition, Industrial Kit
  gantry looks) + pad infrastructure.
- **Flight HUD v1:** altitude (sea+radar), vspeed, velocity, attitude,
  throttle, live TWR + per-stage Δv/fuel, staging strip (reorder works in
  flight), G-load + stress warnings, peripherals panel dock. One visual
  language with the builder; UI/interface sound packs wired in.
- **Missions v1** (Ops board): #1 "Reach 2,000 m and land intact under
  canvas" → ladder of altitude/distance/return-sample missions, all data
  (`missions/*.lua`).
- **Economy live:** starting balance, Commerce purchases, warehouse,
  recovery refunds. **Save setup screen** (company/planet name, toggles,
  presets).
- Home-base exterior v1 on the current planetoid (placeholder until SC3).
**Playable proof:** fresh save → build → fly mission #1 → get paid. Dark
proof: over-throttle a top-heavy stack — it snaps at the link the builder
warned about, the tank craters the pad, the struts just clatter.

### SC3 — Terra: the Earth-like home planet
- `[engine]` **Biome generation:** latitude temperature + noise moisture →
  biome per column (grass fields, forest, desert, snow, rocky highlands);
  drives splat palette, relief character, scatter tables. Master-seeded.
- `[engine]` **Scatter & instancing** (§2): trees/bushes/rocks/grass as
  GPU-instanced meshes, deterministic per chunk, density by biome, distance
  fade; props sit on the meshed surface; dig under one → it drops/despawns.
- `[engine]` **Terrain stamp API** (§2): flatten-fit for surface structures,
  **carve-and-fit buried interiors** (walls meet dirt gap-free, flush
  hatches). Ruined labs, bunkers, cave pockets with loot. Digging into a
  buried lab and walking in is the showcase moment.
- **Terra replaces the planetoid as home:** breathable-O2 atmosphere flag,
  four+ biomes, home base nestled in grassland. Terrain P5 streaming lands
  here if Terra's radius demands it.
**Playable proof:** hop through 4 biomes, find a buried structure from a
surface hint, dig in, loot it.

### SC4 — Digging 2.0 + the material economy *(free digging dies here)*
- **Mining laser as equipment:** handheld (Blaster Kit body, inventory
  weight) and **ship-mounted part**. Beam = raycast + small-radius sculpt at
  a balanced rate (~10× slower than today's brush; tiers widen later).
  First/third-person aim; ship-mounted = operate mode with line of sight.
  The beam itself is a showcase `.flsl` (§9).
- `[engine]` **Sculpt yield reporting** (§2): removed volume per palette
  slot → material units.
- **The suck-up:** dug matter streams to the digger as a particle ribbon.
- **Inventory & weight:** astronaut carry limit; materials weigh by
  registry. Ship cargo arrives with SC7 containers.
- **Peripherals menu v1** (the pattern everything reuses): piloting → ship
  systems list → select → operate mode; Esc back. Data-driven from parts.
- **Commerce sells materials;** first sale fires the discovery milestone.
**Playable proof:** laser a tunnel, watch ore stream in, fly home heavy,
sell, watch "NEW DISCOVERY: Iron" pop an unlock.

### SC5 — The astronaut is a person now
- `[engine]` **GPU vertex skinning** (§2) — the pending anim render path.
- **Character:** `character_retro.glb` (rigged, idle/jump/run shipped;
  author walk/fall/dig/pilot clips on the same rig). Skin swaps for crew
  variety; vertex-paint suit accents; visible **jetpack** unit; helmet lamp.
- **Life support:** health; suit O2 (minutes, refills at cabins/bases);
  cabin O2 pools per crewed part; breathable atmospheres refill free;
  non-O2 atmospheres = suit-only. Cabin empty → suits drain → death. HUD
  integration. **Jetpack:** weighty wearable, RCS impulses, hover assist.
- Death: respawn at base, reputation −1, body recoverable. All toggleable.
**Playable proof:** EVA an airless moon on suit O2, jetpack a cliff, run
dry almost-home, die; the company feels it.

### SC6 — Research, unlocks, and electricity
- **Catalogue gating live** ("Discover uranium", "Return a moon sample",
  "Rep ≥ 3" — every locked card says why).
- **Electrics v1:** batteries, **solar panels** (sun-angle honest), RTGs
  later; parts declare draw/output; per-vessel charge pool v1.
- **Builder systems layers** (the modeling-tool bet pays off): toggleable
  overlay tabs — **Structure / Fuel & gas / Power & control**. Fuel-flow
  view shows plumbing + crossfeed; power view shows draw vs. output per
  stage; **control view = wiring**: bind parts to action groups and
  **custom keybinds** (peripheral events, lights, drills, panel deploy…)
  drawn as clean node-wire overlays (pattern-pack lines as the visual
  language). Action groups are part of the blueprint.
- **Antennas + data transmission:** instruments produce data; antennas
  transmit at range-based rates for discounted discoveries; **science
  compartments** (service bays holding instruments/batteries — the fairing
  compartment parts from the brief).
- **Remote research:** Lab part + astronaut + time = analyze far samples,
  transmit home — the one-way outpost fantasy, honestly costed.
- **Reputation effects on prices; lawsuit hook v0** (a death on a mission
  that was unsurvivable-by-plan files a suit — money + a sardonic letter).
**Playable proof:** uranium exists only on the red moon; scan → lab →
transmit from a solar-powered outpost you keybound the lights of.

### SC7 — Storage, manufacturing, and building anywhere
- **Storage compartments:** capacity by volume+mass, hold parts AND
  materials; access piloting (peripherals) or on foot; astronaut ↔ crate ↔
  rover transfer chains.
- **Field assembly:** build FROM a container anywhere — SC1's builder
  re-hosted in-world (terrain-placement rules, reach limit). Erect frames,
  bolt on machines, expand later.
- **Base machines** (Factory Kit): **refinery** (ore→ingots/fuel — mine
  fuel on location!), **water extractor** (ice/aquifer→water→O2/H2),
  **chem plant** (recipes incl. §Arc III stimulants), **manufactory**
  (components→parts: the first crack in the catalogue's monopoly), powered
  drill rig, floodlights. Power from SC6 rules.
**Playable proof:** rover two crate-loads to a canyon, assemble a drilling
+ refining outpost, come home to hoppers of fuel you didn't buy.

### SC8 — Air, enclosure, and agriculture
- `[engine]` **Volume enclosure + per-room channels** (§2): flood-fill
  sealed-room detection vs. colliders+terrain, incremental re-check; sealed
  volume + sources/sinks = per-room O2/pressure/poison numbers.
- **Airlock part** (two doors + cycle); venting drama included.
- **Agriculture:** plant registry — growth conditions (light, temp,
  pressure, soil material) and outputs (O2, food, or **poison gas** for the
  weirder xenoflora). Planters + grow lights (Food Kit for produce).
  Research tells you if an alien seed is a garden or a hazard — or find
  out the hard way.
**Playable proof:** seal a buried base, plant unresearched alien flora,
watch the poison channel climb, cycle the airlock in a suit, fix the farm.

### SC9 — Wheels: rovers, trucks, cave crawlers
- `[engine]` **Raycast vehicle** (§2): spring/damper suspension, drive
  torque, steering, friction curves; rover vs. road wheel variants.
- Driving = ground sibling of the ship controller; peripherals identical
  (drill trucks!).
- **Cave crawler** (unlock showpiece): grip wheels + surface-normal local
  gravity while in contact — drive cave ceilings, carve helixes, haul ore.
**Playable proof:** truck a crate between outposts; descend a cave on the
ceiling, floodlights on.

### SC10 — Identity checkpoint *(the game gets its face — not the end)*
- **Name the game.** Title screen, save-slot UI, onboarding polish of
  missions 1–3, trailer route, README tour.
- **Look pass:** per-biome/planet palette direction, painterly sky passes,
  the surreal-vibrant grade applied deliberately; style-guide doc.
- **Sound pass:** ambience layers per biome/altitude/depth — birdsong, wind,
  cave pressure-hum, and **nothing at all** in space (radio crackle, your
  own breathing at low O2); rare unexplained sounds deep underground.
- Performance checkpoint at Terra scale (probes, budgets).
**Playable proof:** a stranger plays 45 minutes, understands the loop, and
turns the music down to listen — worried.

## 7. Arc II — The Galaxy *(scale + immersion: SC11–SC13)*

### SC11 — Interiors: first-person everywhere
- `[engine]` **Carrier frames** (§2): characters (and loose physics props)
  simulate *local to a moving body* — walk a ship under thrust, a station
  in orbit, an elevator, a rover bed. This is the phase's engine core and
  one of the most broadly reusable features the engine will ever ship.
- **Pod experience:** first-person seat view inside Pod Mk1's interior
  shell — instruments readable diegetically (screens = UI layers; camera
  feeds later via A1 render targets).
- **Walkable interiors:** larger crewed parts (Space Kit corridors, Station
  kits) have real interiors — get up mid-flight, float/walk the corridor,
  look out the window while the autopilot holds.
- **Stations:** build station blueprints in the same builder (station
  modules are just parts), assemble in orbit (field assembly from cargo),
  dock ships (docking port part + soft-capture), walk the whole complex.
**Playable proof:** undock from a station you built, watching it recede
from inside your pod — then get up and watch from the corridor window.

### SC12 — The infinite galaxy *(the Minecraft-of-space bet)*
- `[engine]` **Hierarchical large-world coordinates + seeded regions** (§2,
  per `galaxy-streaming-proposal.md`): galaxy = deterministic seed → sector
  grid → star systems generated on first approach (bodies, elements,
  palettes, biomes, life flags); visited-system deltas persist via save.*;
  everything else regenerates from seed. Real positions all the way —
  leaving means **deep space** (rails to nothing, stars sliding), and
  coming home is a *navigation problem you can actually solve*.
- **Deep-space travel & navigation gear:** high-tier engines/drives make
  interstellar hops practical (days of warp, not seconds — space stays
  big); **nav computer part** (system bookmarks, dead reckoning), **deep
  antenna** (home fix at any range — IF powered), star charts UI. Without
  gear, you can genuinely get lost; the toggle sheet decides how cruel
  that is.
- **Galaxy map** (G3 workstream): zoom trajectory map → system → sector →
  galaxy; discovered systems named/annotated by the player.
- Per-system character: star class tints (shader-driven), body variety
  knobs widen with distance from home (weirder = farther = richer).
**Playable proof:** point at a dim star, warp for a real while through
true dark, arrive at a system no one generated before you, name it, mine
something unknown, and navigate home on instruments.

### SC13 — Expansion: the company outgrows its cradle
- **Remote shipyards:** unlockable Assembly facility as a *buildable*
  (manufactured parts + a big mission chain) — full builder anywhere.
- **Comm grid:** satellites + relay parts form a **line-of-sight relay
  network** across systems (Lua graph over engine raycasts/rails);
  data/discoveries/market access flow only where your grid reaches —
  infrastructure as progression.
- **Logistics maturity:** freight ships, standing transfer routes between
  your bases (abstracted background haulage on rails once a route is
  proven manually).
**Playable proof:** found a shipyard two systems out, linked home by your
own relay chain, and launch a ship your home base never saw.

## 8. Arc III — Life *(the universe pushes back: SC14–SC15)*

### SC14 — Xenoflora + full harvestability
- Every scattered prop is harvestable (§2 scatter system gains a harvest
  hook): wood, fibers, alien biomass → materials → processing chains.
- **Xenobotany depth:** per-planet generated flora species (conditions,
  outputs, hazards) from the system seed; greenhouse-able after research;
  showcase `.flsl` per family (§9 — bioluminescence, breathing membranes).
- **Stimulants & chemistry (the gritty drawer):** chem plant recipes —
  astronaut performance boosters (speed, carry, O2 efficiency) with real
  costs: crash windows, health hits, addiction flags that feed Arc IV
  (media loves a scandal). Toggleable, as everything.
### SC15 — Xenofauna
- `[engine]` **Population streaming** (§2): budgeted, seeded spawning near
  players; despawn far; deterministic per region.
- **Generated species per living world:** conditions decide *if* life, seed
  decides *what*: respiration (O2 / CO2 / none), locomotion, diet, temper,
  size. Behavior = Lua state machines on physics/raycasts (graze, flock,
  flee, stalk, ambush); rigs from a parts-based creature skeleton set +
  skinning.
- **The full chain, no flinching:** creatures have health; can be killed,
  **harvested, processed** (meat, hides, chemistry precursors), researched
  (live study > dissection for data value), bred (Arc IV contracts care).
  Some are dangerous. Some places, *you* are the prey — the horror tenet
  gets teeth here, sparingly.
**Playable proof:** a living world: herds at dawn under two moons; you can
photograph them for science, or render them into engine-grease — and the
game only *watches* which one you choose. (Arc IV makes the watching real.)

## 9. Shader showcase map *(tenet 8 made concrete — grows every phase)*

Engine plumes (throttle-reactive, atmosphere-pressure-flared), the mining
beam + suck-up shimmer, parachute canvas, fairing-separation heat ripple,
reentry plasma sheath, per-class explosion looks, solar-panel iridescence,
station-window interior glow, warp-space (deep-space starfield compression
+ the "wrongness" grade far from home), star-class system tints, atmosphere
scattering per body, alien flora families (bioluminescence, membranes),
creature skins/eyes, aurora + weather cells, CRT stock terminal + facility
screens (`stage ui`), the drug-crash vignette, cave depth-fog. Each ships
as a readable example `.flsl` — the game is the shader system's portfolio.

## 10. Arc IV — Society *(the company sim: SC16–SC17)*

*Pure Lua + UI + save.* on engine APIs — zero engine work in this arc.*

### SC16 — Media, law, and the public company
- **Event bus → press:** deaths, crashes, rescues, discoveries, emissions,
  alien kills, breeding programs, bombings — observable events generate
  headlines with sentiment; media sentiment × reputation drive prices,
  mission payouts, **sponsorship offers** (clean-record contracts with
  stipends and stipulations).
- **Lawsuit system v1** (from the v0 hook): negligence suits (unsurvivable
  mission plans), environmental suits, alien-welfare suits — settlements
  cost money, verdicts cost reputation; a good lawyer retainer is a real
  line item. Quirky-dark in tone, mechanically honest.
### SC17 — Competitors + the stock market
- **Generated competitor companies** with value systems (alien-rights,
  emissions-hawk, profit-blind…) simulated as background curves + events —
  they grow, dip, scandalize, recover; they do NOT touch your planets'
  terrain (simulation, not simulation *of the world*).
- **Their footprint:** findable facilities on some worlds (stamp API +
  scatter) — visit, trade… or **bomb them** (ordnance parts exist by now;
  destruction is destruction) → their stock craters, you profit if you
  shorted — and the media/law/insurance world responds at full volume.
  Freedom with consequences, never walls (§4.6).
- **The exchange (Cruelty-Squad-flavored CRT terminal** at Commerce §9):
  see your listed company + competitors; buy/sell/short; **selling to a
  company binds you to its values** as supply contracts (alien-welfare
  clauses, emissions caps, fuel-type bans) — breach = penalties, not
  refusal. Your in-game actions visibly move your own ticker.
**Playable proof:** two saves, same galaxy seed: one company clean, courted
by sponsors; one rich, radioactive, and in court — both fun, both viable.

## 11. Arc V — Together *(SC18: bring your company to a friend's galaxy)*

- **The Terraria model** (per Ty): your save's *company* — parts, research,
  money, standing, blueprints — travels; the host's *world* hosts. Guest
  facilities spawn at a clean, procedurally chosen fresh site (stamp API,
  far enough to never disturb host builds).
- **World sync = big-bang streaming:** join loads inside-out — you're
  walking the guest pad within seconds while distant regions stream by
  priority (near > visited-by-host > seed-only; seed-only regions need
  almost no bytes — the infinite galaxy is nearly free to sync, only
  *deltas* travel). Rides netcode 2a–2f (prediction, interest management)
  + the §2 region-streaming feature over the wire.
- Scope guard: co-op visits first (2–4 players); persistent shared worlds
  and Floptle Cloud relays are the follow-on, on the netcode roadmap's
  schedule, not this doc's.

## 12. Save-creation toggles (grows with each arc)

Money & costs · unlock gating · life support · dig yields & carry weight ·
aero/stress/crash damage · reputation & lawsuits · media & sponsors · stock
market & competitors · stimulant side-effects · hostile fauna · getting
lost (nav aids always-on vs. earned) · mission board · starting funds ·
presets: **Career** (all on) / **Sandbox** (all off, all unlocked) /
**Custom**.

## 13. Sequencing & risks

- Arc I order is Ty's priority order; compound bodies (SC1) are the
  riskiest engine piece and go first on purpose. SC3/SC4 can swap if Terra
  runs long; SC5 skinning parallelizes safely.
- **Carrier frames (SC11) is the second-riskiest engine feature** — start
  its design spike during Arc I polish; it must not be invented under
  deadline. Enclosure sim (SC8) stays deliberately coarse in v1.
- Galaxy coordinates (SC12) build on the floating origin + rails that
  already work; the proposal doc exists — the risk is save-format churn,
  so the region/delta format gets designed (not built) alongside SC2's
  save-setup work, and `.ship.ron`/`company.ron` schemas get a version
  field from day one.
- Arcs III–V are almost entirely Lua/content — the engine risk front-loads
  into Arcs I–II, which is exactly where we want it (§2).
- Multiplayer intent shapes earlier choices *now*: deterministic seeds
  everywhere, deltas-not-snapshots persistence, company-vs-world data
  separation from SC2 onward — cheap disciplines today, priceless at SC18.

## 14. Open decisions for Ty (defaults chosen, flag if wrong)

1. ~~Builder camera~~ **Resolved (2026-07-21): free-fly** with
   focus-orbit-on-demand; no vertical-axis lock.
2. ~~Ship physics fidelity~~ **Resolved (2026-07-21): welded-rigid +
   analytic per-link stress + material-class destruction.**
3. **Fuel routing:** per-stage pooled (default) vs. per-tank plumbing with
   crossfeed — SC6's fuel-flow layer can upgrade to real plumbing later;
   starting pooled keeps SC2 shippable.
4. **Terra radius:** ~600 units (default) vs. bigger-with-P5-first.
5. **Interstellar travel time:** minutes-of-warp default (space feels big,
   sessions stay sane) vs. real-hours hardcore as a toggle.
6. **Fauna violence ceiling:** harvesting is in (per brief); default blood
   & gore level = stylized, with a toggle (media/lawsuit systems make it
   *matter* either way).
7. **Game name:** still open — the answer probably lives somewhere near
   what we call the galaxy.
