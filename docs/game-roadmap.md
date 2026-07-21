# Floptle Solar — Game Roadmap: from tech demo to a space-company game

*2026-07-21. Companion to `solar-demo-plan.md` (which it supersedes as the
forward plan — S0–S6 shipped, S7–S9 fold into the phases below) and
`engine-roadmap.md` (engine workstreams referenced throughout). Direction per
Ty: turn the demo into a complete game with an identity — a fun, stylized,
surreal **space company sandbox** where you build ships, mount expeditions,
dig into planets, haul back materials, and grow your capabilities — explicitly
NOT grounded in realism, and explicitly more than KSP's build-and-fly loop.*

## 1. The game in one paragraph

You run a tiny spacefaring company from a home-base facility on an Earth-like
planet you name yourself. You assemble ships from parts in a builder that
respects your freedom (no root-pod tyranny), fly real missions, and — the
part no other space sim does — get out and **work** the destination: dig with
mining lasers, explore caves, extract rare materials, plant bases, grow food
and oxygen, and haul finds home to sell. Money buys parts; **discoveries
unlock them**. Rarer materials live farther out, so your reach and your
catalogue grow together. Reputation follows how you treat your astronauts.
The universe is vibrant and surreal, and just a little wrong — the beauty of
deep space with an undertone of "you should not be this far from home."

**The loop:** BUILD → EXPEDITION (fly / land / dig / research) → RETURN or
TRANSMIT (sell, bank discoveries) → UNLOCK (parts, machines, capabilities) →
repeat, farther out, deeper down.

## 2. Where we stand (inputs to this plan)

Already shipped and load-bearing: Kepler rails + patched conics + time-warp,
pilotable ship with fuel/legs/wrecks, trajectory map, dig/terrain 2.0 splat,
prefabs + Lua (scheduler, handles, spawn, save.*, layers, raycasts), physics
(body modes, triggers, gravity volumes, events), particles + audio + shader
graph + vertex/texture paint, UI system phases 1–2 plus buttons/sliders/
`onClick`/noderef Lua slices, persistence (`save.*`, slot files).

Assets now in `solar/models/`: **buildings/** — Kenney City Kit Industrial,
25 GLBs + colormap (CC0, license file alongside) for the home-base facility;
**astronaut/** — `astronaut_male.glb` / `astronaut_female.glb`, 410-vert
rigged humans (25 bones, UVs, no clips/textures — Clint Bellanger, CC-BY-SA,
CREDITS.txt alongside). Still wanted: **Kenney Space Kit** (CC0; 150 models —
rocket parts, corridors, rover bits) for ship parts v1 — the site download is
JS-gated, grab it manually from kenney.nl/assets/space-kit; until then parts
prototype fine as engine primitives + .flsl materials.

Engine gaps this game forces (the honest list — each is scheduled in a phase):

| Gap | Needed by | Notes |
|---|---|---|
| **Compound rigidbodies** (multi-shape, composed mass/inertia, runtime split) | SC1 | bodies are single-shape today; a ship IS a compound; decouplers split it |
| **Aero drag + parachute forces** | SC2 | atmosphere exists visually; no flight forces yet |
| **Biome-aware planet gen + prop scattering (instancing)** | SC3 | planetoid gen is single-character; no instanced foliage path |
| **Structure embedding in terrain** (CSG stamp at gen; buried interiors) | SC3 | mesh + SDF must agree so digging into a lab feels solid |
| **Terrain P5** (streaming/quantized band/shadow clipmap) | SC3 | Earth-scale home planet wants it; signed off already |
| **Voxel material yield API** (what did this dig remove, how much) | SC4 | palette slots already are materials; sculpt needs to report volume-by-slot |
| **GPU vertex skinning** | SC5 | rigs animate but skinned meshes don't deform on GPU yet |
| **Raycast vehicle** (wheels, suspension) | SC9 | joints not required if wheels are raycast-based (stabler anyway) |
| **Room/enclosure detection + simple gas state** | SC8 | flood-fill against colliders+terrain; drives breathability |

Everything else below is game-side Lua/content on existing engine APIs.

## 3. Design tenets (from Ty's brief — these outrank any phase detail)

1. **Builder freedom.** No part is special. Move any part or subtree freely;
   grab the whole ship and reposition it; the "ship" is just the connected
   graph. A ship without a pod is a valid (unpilotable) ship.
2. **Balanced for fun, not realism.** Part stats serve progression pacing.
   No real-engine cosplay. Rule of thumb: first-tier parts make wobbly,
   charming successes possible in ~15 minutes of building.
3. **Progression = discovery.** Money is the throughput; discoveries are the
   gates. Every locked part states its requirement in plain words
   ("Discover uranium", "Return a sample from a moon", "Reach reputation 3").
4. **Everything's a toggle.** Any rule that shapes playstyle (life support,
   money, unlock gating, dig yields, lawsuits, damage) is a save-creation
   option. **Sandbox mode** = the preset with everything free and unlocked.
5. **The astronaut is a real character**, not a camera: inventory with
   weight, health, oxygen, tools in hand, visible jetpack — and losing one
   should hurt (reputation, and eventually lawsuits).
6. **Surreal-vibrant identity with quiet dread.** Stylized color, painterly
   skies, retro meshes — and scale/silence/alienness used deliberately.
   Ambience gets its own phase; it is not an afterthought.
7. **UX bar (standing):** nothing moves on its own, stable popups, wheel
   zoom / middle pan / box select + multiselect in every canvas-like tool
   from its FIRST cut — the builder is exactly such a tool.

## 4. The economy spine (shared vocabulary for every phase)

- **Materials** (`materials.lua` registry): id, display name, tier,
  weight-per-unit, base value, rarity class, where-found hints. Terrain
  palette slots map to material ids per planet; digging yields by voxel
  volume removed × material density.
- **Money.** One currency. Starting balance covers a minimal first ship
  (pod + small tank + small engine + chute) with almost nothing spare.
- **Catalogue.** Every part: cost, mass, tier, unlock requirement (nil =
  starter). Bought parts go to the **warehouse** (home-base stock); building
  consumes warehouse stock, recovering/scrapping returns it.
- **Discoveries & milestones.** First-time events (material sold or data
  transmitted, place reached, mission done) grant research milestones;
  milestones satisfy unlock requirements. Transmitting data from afar
  yields the discovery at reduced payout vs. hauling the sample home.
- **Reputation.** 0–5 stars. Mission success and safe returns raise it;
  astronaut deaths and abandonments drop it. Buys better prices both ways.
  (Lawsuits — SC6+ flavor — are reputation's sharp edge.)
- **Save setup** (`company.ron` via `save.*`): company name, home planet
  name, difficulty toggles (§3.4), mode preset (Career / Sandbox / Custom).

## 5. Phases

Each phase ends **playable**. Engine work is tagged `[engine]`; the rest is
solar/ Lua + content. Order is Ty's priority order — builder first.

### SC1 — Ship Builder v1 *(the centerpiece — get the feel right)*
The builder is a dedicated interior scene at the Assembly Building (Kenney
kit facility set-dressing), entered by walking up / from the base menu.
- **Part graph, no root.** A blueprint is `{ parts: [{id, part, pos, rot}],
  links: [{a, node_a, b, node_b}] }` (`.ship.ron` next to saves). Any
  connected subgraph is a ship; the builder tolerates floating islands
  (they highlight amber and simply don't launch with the stack).
- **Placement UX:** pick from catalogue panel → part follows cursor →
  attach-node snap (green ghosts) or free surface-attach; click to place.
  Drag any part = move its subtree; **grab-anywhere whole-ship move**
  (hotkey G / dedicated button); wheel zoom, middle pan, box-multiselect,
  del/duplicate, full undo/redo — the standing canvas bar (§3.7).
  Rotation with R/T around view axes; fine-rotate holding Shift.
- **Symmetry v1:** radial ×2/×3/×4/×6 around the part being attached to.
- **Stats readout:** mass, cost, Δv per stage (simple Tsiolkovsky from part
  stats), TWR vs. home gravity — live, in a corner, no popups.
- **Staging strip:** vertical stage list (bottom-right, KSP-familiar);
  engines/decouplers/chutes auto-file into stages, drag between stages.
- **Starter parts (primitives + .flsl until Space Kit lands):** Pod Mk1
  (crew 1, hatch), FT-S / FT-M fuel tanks, "Sputter" small engine, "Anvil"
  medium engine, stack decoupler, parachute, fixed landing legs, ladder.
- `[engine]` **Compound bodies:** one dynamic rigidbody from N shapes with
  composed mass/CoM/inertia; runtime **split** into two bodies along a link
  (decoupling, breakage); per-shape contact attribution (later: damage).
- **Launch:** blueprint → assembled compound on the pad, warehouse stock
  debited; fly it with the existing ship controller reading the graph
  (engines = thrust points, tanks = fuel pool per stage).
**Playable proof:** build a 6-part rocket three different ways, move the
whole thing mid-build, launch it, stage the chute off, land on legs.

### SC2 — First mission & the money loop
- `[engine]` **Aero forces:** density from the existing atmosphere params ×
  velocity² drag on the compound (per-part drag stat, dumb and tunable);
  **parachute part** = staged drag spike with deploy animation + jolt;
  chutes shred above a speed threshold (teaches "slow down first").
- **Missions v1** (mission board at the Ops building): scripted checks over
  existing Lua state — #1 "Reach 2,000 m and land intact under canvas"
  (grants money + unlocks FT-M), then altitude/distance/return-sample
  ladder. Missions are data (`missions/*.lua` returning condition fns).
- **Economy live:** starting balance, part purchase at the **Commerce
  building**, warehouse stock, recovery refunds (distance-discounted).
- **Save setup screen** (new save → company name, planet name, toggles,
  Career/Sandbox preset) writing `company.ron`; Sandbox = all parts, money
  off, gates off.
- Home-base exterior v1: pad, Assembly, Commerce, Ops from the Kenney kit
  on the current planetoid (placeholder planet until SC3).
**Playable proof:** fresh save → name company/planet → buy → build → fly
mission #1 → get paid → afford the part you couldn't before.

### SC3 — Terra: the Earth-like home planet *(planet gen v2)*
- `[engine]` **Biome generation:** latitude temperature curve + noise
  moisture → biome id per column (grass fields, forest, desert, snow/ice,
  rocky highlands); biome drives palette-slot painting (the 32-slot splat
  carries it), relief character, and scatter tables. Master-seeded like
  `gen_solar`.
- `[engine]` **Prop scattering + instancing:** per-chunk deterministic
  scatter (trees, bushes, rocks, grass tufts) as GPU-instanced meshes with
  distance fade; density from biome; props sit ON the meshed surface
  (surface-nets normals already there). Dig under a prop → it drops/despawns
  (v1: despawn).
- `[engine]` **Structure embedding:** generation-time stamps — flatten pad
  for surface structures, **carve-and-fit for buried ones** (interior shell
  subtracted from the field so walls meet dirt with no gaps; door/hatch
  flush with terrain). Ruined labs, bunkers, cave pockets with loot tables.
  Digging to a buried lab and walking in must "just work" — this is the
  showcase moment of the phase.
- **Terra replaces the starter planetoid** as home: breathable-oxygen
  atmosphere flag (SC5 reads it), mountains/forests/deserts/tundra, the
  home base nestled in a grass biome. Planetoid & moons remain as system
  siblings. Terrain P5 (streaming/quantized band) lands here if memory
  bites at Terra's radius.
**Playable proof:** circumnavigate by plane-less rocket hops through 4
distinct biomes, find a buried structure by its surface hint, dig in.

### SC4 — Digging 2.0 + the material economy *(free digging dies here)*
- **Mining laser as equipment:** handheld tool (astronaut inventory, has
  weight) and **ship-mounted part**. Beam raycast + small-radius sculpt at a
  **balanced rate** (order-of-magnitude slower than today's brush; upgrade
  tiers widen radius/rate later). First/third-person aim; ship-mounted =
  operate-mode aim with line-of-sight.
- `[engine]` **Sculpt yield API:** `terrain.dig(...)` returns removed volume
  per palette slot → material units. (Engine change is small: the sculpt
  already knows what it wrote; report it.)
- **The suck-up:** dug material streams to the digger as a particle ribbon
  (existing particle system, world-space clips) → inventory.
- **Inventory & weight:** astronaut carry limit; materials weigh by
  registry; over-weight = can't take more (v1; movement penalty later).
  Ship cargo via SC7 containers — until then, suit-carry only.
- **Peripherals menu v1** `[pattern for everything later]`: while piloting,
  a ship-systems list (drill, lights, …) → select → **operate mode** takes
  input focus; Esc returns to flight. Data-driven from parts on the ship.
- **Commerce sells materials:** rarity-classed pricing, first-sale-of-a-
  material fires its discovery milestone (SC6 consumes these).
**Playable proof:** laser a tunnel, watch ore stream in, fly home heavy,
sell, and see "NEW DISCOVERY: Iron" pop the first unlock.

### SC5 — The astronaut is a person now
- `[engine]` **GPU vertex skinning** (the pending anim path) so the rigged
  astronauts deform; author idle/walk/run/fall/dig clips in Blender on the
  25-bone rig (they ship with the project; CC-BY-SA credits stay).
- Astronaut visuals: vertex-paint suit variants; **visible jetpack** unit
  when worn; helmet lamp.
- **Life support:** health; suit O2 (minutes-scale, refills in cabins/
  bases); cabin O2 pools per crewed part; breathable-atmosphere planets
  refill free air; non-O2 atmospheres = suit-only. Cabin empty → suits
  drain → death. HUD: O2/health beside fuel.
- **Jetpack:** wearable with weight, RCS-style impulse + hover assist,
  fuel from suit reserve.
- Death: respawn at base after a beat; reputation −1; body recoverable
  (flavor, later). All of it toggleable (§3.4).
**Playable proof:** EVA on the airless moon on suit O2, jetpack up a cliff,
run dry almost-home, die, watch the stars dim on your company.

### SC6 — Research, unlocks, and reputation *(the progression web)*
- **Catalogue gating live:** parts carry requirements; locked cards show
  them ("Discover uranium", "Return a moon sample", "Rep ≥ 3").
- **Antennas + data transmission:** science instruments (sampler, scanner)
  produce **data**; antennas transmit home at range-based rates for the
  discovery at a payout/milestone discount vs. physically returning it.
- **Remote research:** the Lab part + an astronaut + time = local analysis
  of far-away materials; transmit results via antenna — the "one-way
  science outpost" fantasy from the brief, done honestly.
- **Reputation effects:** price multipliers both directions; a couple of
  rep-gated missions/parts. **Lawsuit hook (flavor v0):** a death where the
  mission plan was unsurvivable (no return fuel margin, no O2 source at
  destination) files a suit — money hit + a sardonic letter. Toggle.
**Playable proof:** uranium exists only on the red moon; scan it, lab it,
transmit it, watch the nuclear-tier engine unlock from four planets away.

### SC7 — Storage, logistics, and building anywhere
- **Storage compartment parts:** capacity by volume+mass; hold parts AND
  materials. Access while piloting (peripherals menu) or on foot (interact).
  Transfer astronaut ↔ container ↔ container (rover ferrying = the brief's
  exact scenario).
- **Field assembly:** build FROM a container's stock anywhere — the SC1
  builder UI re-hosted in-world (place-on-terrain rules, reach limit from
  the container). Erect a base frame, bolt on machines, expand later.
- **Base machines (peripheral pattern from SC4):** oxygen generator,
  refinery (ore → ingots/fuel), powered drill rig (slow autonomous dig into
  a hopper), floodlights. Power stub: machines just need a generator part
  present (real grid later if wanted).
**Playable proof:** rover two container-loads to a canyon, assemble a
drilling outpost from them, come back to full hoppers.

### SC8 — Air, enclosure, and agriculture
- `[engine]` **Enclosure test:** seeded flood-fill (coarse voxel walk
  against colliders + terrain) marks a room sealed/leaky; incremental
  re-check on nearby edits. Sealed volume + O2 sources vs. crew/plant
  consumption = a per-room gas state (O2 %, pressure, one poison channel —
  deliberately simple, readable numbers).
- **Depressurizing room part** (airlock): two doors + cycle button; opening
  a sealed room straight to vacuum vents it (alarm, drama).
- **Agriculture:** plant registry with growth conditions (light, temp,
  pressure, soil material) and outputs (O2, food later, or **poison gas**
  for the weirder xenoflora). Planters + grow lights; alien seeds are
  findable samples — research one to learn if it's a garden or a hazard,
  or find out the hard way.
**Playable proof:** seal a buried base, plant unresearched alien flora,
watch the poison channel climb, cycle the airlock in a suit, fix your farm.

### SC9 — Wheels: rovers, trucks, cave crawlers
- `[engine]` **Raycast vehicle:** wheels as raycast suspension on the
  compound body (no joint physics needed — stabler and cheaper): spring/
  damper, drive torque, steering, friction curves. Rover wheels (soft,
  grippy) vs. road wheels (fast, fragile) as part variants.
- Driving = the ship controller's ground sibling; peripherals work
  identically (drill trucks!).
- **Cave crawler (unlock showpiece):** grip-wheel vehicle that sticks to
  cave walls/ceilings under a surface-normal-aligned local gravity while in
  contact — carve a helix down, drive it hauling ore. Unashamedly fun,
  gated deep in the catalogue.
**Playable proof:** truck a container between two outposts; descend a cave
on the ceiling with floodlights on.

### SC10 — Identity, ambience, and the quiet dread
- **Name the game** (it stops being "the solar demo"). Title screen, save
  slots UI, onboarding polish of missions 1–3.
- **Look:** per-biome and per-planet palette direction, painterly sky
  passes per atmosphere, bloom/vignette tuning, the surreal-vibrant grade
  applied deliberately across the system (style guide doc + example
  scenes).
- **Sound:** ambience layers per biome/altitude/depth — birdsong to wind to
  the pressure-hum of caves to **nothing at all** in space (radio crackle,
  your own breathing when O2 runs low); rare distant not-quite-geological
  sounds deep underground. The horror is subtle: scale, silence, and things
  that are merely *unexplained*.
- Performance/perf-probe pass at Terra scale; trailer route; README tour.
**Playable proof:** a stranger plays 45 minutes, understands the loop, and
at some point turns the music down to listen — worried.

## 6. Sequencing rationale & risks

- SC1 before everything because Ty asked for it first and because compound
  bodies underlie SC2 staging, SC7 containers, SC9 wheels — the riskiest
  engine work goes first where the schedule can absorb it.
- SC3 (Terra) before digging economy: yields want biomes/materials to vary;
  but SC4 only *reads* palette slots, so the two can land in either order
  if Terra runs long. SC2's mission #1 needs only atmosphere + parachutes.
- SC5 skinning is isolated render work — safe to parallelize any time.
- Enclosure sim (SC8) is the most experimental engine piece; it's late on
  purpose, and its v1 is deliberately coarse (rooms, not fluid dynamics).
- Kenney Space Kit is a nice-to-have, not a blocker: primitives + .flsl
  materials already look on-brand for the retro-surreal style.

## 7. Save-creation toggles (surfaced at new-company setup)

Money & part costs · unlock gating (discovery web) · life support (O2 /
health) · dig yields & carry weight · aero/chute damage & crash damage ·
reputation & lawsuits · mission board · starting funds slider · preset rows:
**Career** (all on) / **Sandbox** (all off, everything unlocked) / **Custom**.

## 8. Open decisions for Ty (defaults chosen, flag if wrong)

1. **Builder camera**: fixed-interior orbit around the work floor (default;
   KSP-familiar) vs. free-fly. Orbit ships first either way.
2. **Ship physics fidelity**: welded-rigid stacks, no wobble (default —
   wobble is KSP's most-hated tax) vs. flexible joints later for drama.
3. **Fuel routing**: per-stage pooled fuel (default, readable) vs. per-tank
   plumbing with crossfeed rules (sim-ier, fiddlier).
4. **Terra scale**: radius ~600 units (default; ~4× planetoid, biome walks
   feel real but hops stay minutes) vs. bigger with P5 streaming first.
5. **Game name**: working title stays "Solar" until SC10 — but if a name
   strikes you earlier, everything's cheaper to rename now than later.
