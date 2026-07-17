//! The built-in example shaders — seeded into every project's
//! `shaders/examples/` folder (delete the folder and it stays deleted).
//!
//! They're teaching material first: each one is a worked example of a
//! different corner of the system (uniform knobs, blends, engine hooks, the
//! sky and sdf stages), commented for someone who has never written a shader,
//! and laid out to open readably in the graph editor. Every entry is
//! compile-tested.

/// `(file name, source)` for every example. Fragment ones go on mesh
/// Materials; sky ones on the Skybox node (Inspector → skybox → shader);
/// sdf ones on ◈ Field Shape nodes.
pub const EXAMPLES: &[(&str, &str)] = &[
    ("plasma.flsl", PLASMA),
    ("water.flsl", WATER),
    ("lavaFlow.flsl", LAVA_FLOW),
    ("toonPrint.flsl", TOON_PRINT),
    ("hologram.flsl", HOLOGRAM),
    ("dissolve.flsl", DISSOLVE),
    ("forceField.flsl", FORCE_FIELD),
    ("fieldGlow.flsl", FIELD_GLOW),
    ("dayBreeze.flsl", DAY_BREEZE),
    ("sunsetStreaks.flsl", SUNSET_STREAKS),
    ("stormNight.flsl", STORM_NIGHT),
    ("starryNight.flsl", STARRY_NIGHT),
    ("moonlitClouds.flsl", MOONLIT_CLOUDS),
    ("auroraVeil.flsl", AURORA_VEIL),
    ("retroSun.flsl", RETRO_SUN),
    ("nebulaDream.flsl", NEBULA_DREAM),
    ("wobbleOrb.flsl", WOBBLE_ORB),
    ("ringTower.flsl", RING_TOWER),
];

const PLASMA: &str = r#"// Plasma — the classic start. Space is melted by a drifting warp, noise
// fills it with bands, a palette colors them, posterize gives the retro edge.
// Every `uniform` below becomes a live knob in the Inspector.
shader plasma {
  stage fragment
  uniform speed: float = 0.15 range(0, 2)
  uniform swirl: float = 3 range(0.5, 10)
  uniform steps: float = 7 range(2, 16)
  uniform tint: color = #FFFFFF

  let warped = domainWarp(uv, scale: swirl, time: time * speed, strength: 1.2)
  let bands = fbm(warped, octaves: 5)
  let glow = palette(bands + time * speed * 0.3, "sunset")
  let inked = posterize(glow, steps: steps)

  output color = vec4(inked, 1) * tint * instanceColor
}
//@layout { bands: (-1596, 0), glow: (-1140, 0), in.instanceColor: (-456, 256), in.time: (-2280, 0), in.uv: (-2052, 0), inked: (-912, 0), out: (0, 0), u.speed: (-2280, 212), u.steps: (-1140, 256), u.swirl: (-2052, 212), u.tint: (-684, 256), warped: (-1824, 0) }
"#;

const WATER: &str = r#"// Water — drop it on a flat plane (or anything). Warped noise rolls the
// surface, the fresnel angle blends deep water into shallow, crests sparkle,
// and fieldDistance() foams the shoreline wherever the scene's terrain or
// SDF shapes come near the surface — no masks, no setup.
shader water {
  stage fragment
  blend alpha
  uniform shallowColor: color = #3EC6C8
  uniform deepColor: color = #14395C
  uniform speed: float = 0.6 range(0, 3)
  uniform waveScale: float = 4 range(0.5, 20)
  uniform foam: float = 0.5 range(0, 2)
  uniform clarity: float = 0.75 range(0.2, 1)

  let flow = uv * waveScale + vec2(time * speed * 0.13, time * speed * 0.11)
  let waves = fbm(domainWarp(flow, scale: 1.5, time: time * speed * 0.2), octaves: 4) * 0.5 + 0.5
  let crest = smoothstep(0.62, 0.95, waves)
  let facing = pow(1 - saturate(dot(normal, viewDir)), 2)
  let body = mix(deepColor.rgb, shallowColor.rgb, facing * 0.6 + waves * 0.3)
  let shore = 1 - saturate(fieldDistance(worldPos) / (foam + 0.001))
  let lit = litSurface(body + crest * 0.35 + shore * shore * 0.9)

  output color = vec4(lit, clarity + facing * (1 - clarity)) * instanceColor
}
//@layout { body: (-1368, 0), crest: (-1596, 768), facing: (-2052, 0), flow: (-2964, 424), in.instanceColor: (-456, 256), in.normal: (-2964, 0), in.time: (-3876, 0), in.uv: (-3420, 0), in.viewDir: (-2964, 212), in.worldPos: (-2508, 534), lit: (-684, 0), out: (0, 0), shore: (-1596, 1046), u.clarity: (-1368, 790), u.deepColor: (-1824, 0), u.foam: (-2508, 746), u.shallowColor: (-1824, 146), u.speed: (-3876, 212), u.waveScale: (-3420, 212), waves: (-2052, 256) }
"#;

const LAVA_FLOW: &str = r#"// Lava — scrolling, churning noise. Hot veins ride an "ember" palette while
// the cooled crust goes through the engine's real lighting (litSurface), so
// the rock still catches sun, shadows and AO.
shader lavaFlow {
  stage fragment
  uniform flow: float = 0.4 range(0, 2)
  uniform heat: float = 1.4 range(0.2, 3)

  let drift = uv * 3 + vec2(0, time * flow)
  let churn = domainWarp(drift, scale: 2, time: time * flow)
  let veins = fbm(churn, octaves: 5)
  let crust = smoothstep(0.1, 0.7, veins)
  let molten = palette(veins * 0.5 + 0.35, "ember") * heat
  let rock = litSurface(vec3(0.06, 0.04, 0.05))

  output color = vec4(mix(molten, rock, crust), 1)
}
//@layout { churn: (-1824, 0), crust: (-684, 490), drift: (-2052, 0), in.time: (-2736, 0), in.uv: (-2508, 0), molten: (-684, 0), out: (0, 0), rock: (-684, 256), u.flow: (-2736, 212), u.heat: (-912, 256), veins: (-1596, 0) }
"#;

const TOON_PRINT: &str = r#"// Toon — the engine lights your texture (litSurface), then the lit result is
// quantized into flat bands and the silhouette gets an ink edge. Works with
// the node's own base texture: drop one on the Material as usual.
shader toonPrint {
  stage fragment
  uniform bands: float = 4 range(2, 10)
  uniform ink: float = 0.35 range(0, 1)
  uniform tint: color = #FFFFFF

  let albedo = baseTexture() * tint * instanceColor
  let lit = litSurface(albedo.rgb)
  let toon = posterize(lit, steps: bands)
  let edge = pow(1 - saturate(dot(normal, viewDir)), 3)
  let inked = mix(toon, toon * 0.15, step(1 - ink * 0.5, edge))

  output color = vec4(inked, albedo.a)
}
//@layout { albedo: (-1596, 0), edge: (-912, 512), in.instanceColor: (-1824, 256), in.normal: (-1824, 468), in.viewDir: (-1824, 680), inked: (-456, 0), lit: (-1140, 0), out: (0, 0), toon: (-912, 0), u.bands: (-1140, 234), u.ink: (-1368, 256), u.tint: (-2052, 234) }
"#;

const HOLOGRAM: &str = r#"// Hologram — an additive ghost: fresnel rim (bright at grazing angles),
// rolling scanlines over the mesh's UVs, and a nervous flicker.
shader hologram {
  stage fragment
  blend additive
  uniform glowColor: color = #46C8FF
  uniform scanlines: float = 24 range(4, 120)
  uniform flicker: float = 0.15 range(0, 1)

  let rim = pow(1 - saturate(dot(normal, viewDir)), 2)
  let scan = 0.6 + 0.4 * sin(uv.y * scanlines * 6.2832 + time * 3)
  let blink = 1 - flicker * (0.5 + 0.5 * sin(time * 17) * sin(time * 5.3))
  let body = glowColor.rgb * (rim * 1.4 + 0.25) * scan * blink

  output color = vec4(body, 0.8) * instanceColor
}
//@layout { blink: (-912, 256), body: (-684, 0), in.instanceColor: (-456, 256), in.normal: (-2736, 0), in.time: (-2508, 658), in.uv: (-2736, 424), in.viewDir: (-2736, 212), out: (0, 0), rim: (-1824, 0), scan: (-1140, 256), u.flicker: (-1368, 768), u.glowColor: (-1596, 0), u.scanlines: (-2508, 512) }
"#;

const DISSOLVE: &str = r#"// Dissolve — a burn-away cutout. Noise decides which pixels survive as
// `progress` sweeps 0 → 1 (animate it from Lua or scrub it in the Inspector),
// and the surviving rim right at the threshold glows hot.
shader dissolve {
  stage fragment
  blend alpha
  uniform progress: float = 0.35 range(0, 1)
  uniform edgeColor: color = #FF9A3C
  uniform edgeWidth: float = 0.08 range(0.01, 0.3)

  let grain = fbm(uv * 6, octaves: 4) * 0.5 + 0.5
  let alive = step(progress, grain)
  let edge = smoothstep(progress, progress + edgeWidth, grain)
  let albedo = baseTexture() * instanceColor
  let lit = litSurface(albedo.rgb)
  let burning = mix(edgeColor.rgb * 2.5, lit, edge)

  output color = vec4(burning, albedo.a * alive)
}
//@layout { albedo: (-1140, 146), alive: (-684, 1024), burning: (-456, 0), edge: (-684, 490), grain: (-912, 768), in.instanceColor: (-1368, 234), in.uv: (-1824, 0), lit: (-684, 256), out: (0, 0), u.edgeColor: (-1140, 0), u.edgeWidth: (-1140, 548), u.progress: (-1140, 402) }
"#;

const FORCE_FIELD: &str = r#"// Force field — an additive energy shell: cellular "plates" (worley noise)
// crawl over the surface, a pulse rolls down it, and the fresnel rim keeps
// the middle glassy. Try it on a sphere around something worth protecting.
shader forceField {
  stage fragment
  blend additive
  uniform fieldColor: color = #7B5CFF
  uniform cells: float = 5 range(1, 20)
  uniform pulse: float = 1.2 range(0, 4)

  let rim = pow(1 - saturate(dot(normal, viewDir)), 1.5)
  let web = 1 - worley(uv * cells + vec2(0, time * 0.4))
  let wave = 0.5 + 0.5 * sin(time * pulse * 3 - uv.y * 4)
  let energy = rim * 1.2 + web * web * 0.6 * wave

  output color = vec4(fieldColor.rgb * energy, 0.6)
}
//@layout { energy: (-684, 256), in.normal: (-2052, 0), in.time: (-2736, 0), in.uv: (-2508, 0), in.viewDir: (-2052, 212), out: (0, 0), rim: (-1140, 0), u.cells: (-2508, 212), u.fieldColor: (-912, 0), u.pulse: (-2508, 614), wave: (-1140, 512), web: (-1596, 234) }
"#;

const FIELD_GLOW: &str = r#"// Field glow — the hook no other engine hands a shader: fieldDistance() is
// the distance from any point to the scene's SDF field (terrain, blobs,
// Field Shapes). This material grows an aura wherever the scene gets close,
// and sdfAo() concentrates it into crevices.
shader fieldGlow {
  stage fragment
  uniform auraColor: color = #64FFC8
  uniform reach: float = 1.5 range(0.1, 6)

  let albedo = baseTexture() * instanceColor
  let lit = litSurface(albedo.rgb)
  let near = 1 - saturate(fieldDistance(worldPos + normal * 0.05) / reach)
  let cavity = sdfAo(worldPos, normal)
  let aura = auraColor.rgb * pow(near, 2) * (0.4 + 0.6 * cavity)

  output color = vec4(lit + aura, albedo.a)
}
//@layout { albedo: (-1140, 0), aura: (-684, 234), cavity: (-1368, 848), in.instanceColor: (-1368, 234), in.normal: (-2736, 0), in.worldPos: (-2508, 0), lit: (-684, 0), near: (-1368, 592), out: (0, 0), u.auraColor: (-1368, 446), u.reach: (-2052, 234) }
"#;

// ---- sky shaders ------------------------------------------------------------
// `stage sky` shaders paint the environment itself: they get the world-space
// ray direction (`skyDir`) + `time` and return a color, spliced into the
// raymarch's sky. Assign one on the Skybox node (Inspector → skybox → shader).

const DAY_BREEZE: &str = r#"// Day breeze — a bright blue day with cumulus decks scrolling by. The trick
// every cloudy sky here reuses: project the ray onto a flat cloud layer
// (divide xz by height) so the noise gets real perspective, then fade the
// deck out right at the horizon where that projection stretches to infinity.
shader dayBreeze {
  stage sky
  uniform zenith: color = #2A66C8
  uniform horizon: color = #A9CCEA
  uniform cloudColor: color = #FFFFFF
  uniform sunDir: vec3 = vec3(0.55, 0.4, 0.25)
  uniform cover: float = 0.5 range(0, 1)
  uniform drift: float = 1 range(0, 4)

  let up = saturate(skyDir.y)
  let base = mix(horizon.rgb, zenith.rgb, pow(up, 0.55))
  let sun = normalize(sunDir)
  let toSun = saturate(dot(skyDir, sun))
  let disc = pow(toSun, 900) * 1.5 + pow(toSun, 40) * 0.25
  let sp = skyDir.xz / (max(skyDir.y, 0.0) * 2.2 + 0.25)
  let scrolled = vec2(time * 0.035 * drift, time * 0.018 * drift)
  let puffs = fbm(sp * 3 + scrolled, octaves: 5) * 0.5 + 0.5
  let detail = fbm(sp * 9 - scrolled * 2.2, octaves: 4) * 0.5 + 0.5
  let shape = puffs * 0.85 + detail * 0.25
  let hfade = smoothstep(0.015, 0.12, skyDir.y)
  let dens = smoothstep(0.97 - cover * 0.9, 1.13 - cover * 0.9, shape) * hfade
  let flatBase = saturate(detail * 1.6 - 0.45)
  let cloudCol = mix(cloudColor.rgb * 1.04, mix(base, cloudColor.rgb, 0.3), flatBase)
  let silver = pow(toSun, 10) * 0.5
  let sky = mix(base, cloudCol + cloudColor.rgb * silver * dens, dens) + vec3(1, 0.96, 0.86) * disc * (1 - dens)
  output color = mix(sky, horizon.rgb * 0.75, saturate(-skyDir.y * 3))
}
//@layout { base: (-1596, 0), cloudCol: (-1140, 0), dens: (-1368, 1024), detail: (-2280, 256), disc: (-1140, 534), flatBase: (-1368, 534), hfade: (-1596, 1836), in.skyDir: (-4560, 0), in.time: (-4104, 256), out: (0, 0), puffs: (-2280, 914), scrolled: (-3420, 256), shape: (-1824, 1938), silver: (-1596, 1302), sky: (-456, 0), sp: (-3420, 0), sun: (-2508, 256), toSun: (-2052, 526), u.cloudColor: (-1824, 768), u.cover: (-2280, 768), u.drift: (-3876, 512), u.horizon: (-2052, 0), u.sunDir: (-2736, 300), u.zenith: (-2052, 146), up: (-2052, 292) }
"#;

const SUNSET_STREAKS: &str = r#"// Sunset streaks — a three-stop dusk gradient with long cirrus ribbons
// scrolling past a low sun. The streak look is just noise squashed hard in
// one axis (features get long in the other), lit warm on the sun's side of
// the sky and left as dark cutouts opposite it.
shader sunsetStreaks {
  stage sky
  uniform glow: color = #FFA43C
  uniform band: color = #C74B57
  uniform dusk: color = #2B1B4E
  uniform sunDir: vec3 = vec3(0.9, 0.1, 0.15)
  uniform streaks: float = 0.85 range(0, 1)
  uniform scroll: float = 1 range(0, 4)

  let up = saturate(skyDir.y)
  let base = mix(mix(glow.rgb, band.rgb, smoothstep(0.0, 0.3, up)), dusk.rgb, smoothstep(0.12, 0.7, up))
  let sun = normalize(sunDir)
  let toSun = saturate(dot(skyDir, sun))
  let sp = skyDir.xz / (max(skyDir.y, 0.0) + 0.28)
  let ribbons = fbm(vec2(sp.x * 1.5 + time * 0.03 * scroll, sp.y * 6.5), octaves: 4) * 0.5 + 0.5
  let mask = smoothstep(0.48, 0.86, ribbons) * streaks * smoothstep(0.015, 0.12, skyDir.y) * (1 - smoothstep(0.35, 0.75, up))
  let cirrus = mix(dusk.rgb * 0.85, glow.rgb * 1.3, pow(toSun, 2))
  let disc = pow(toSun, 1200) * 1.6 + pow(toSun, 10) * 0.3
  let sky = mix(base, cirrus, mask) + vec3(1, 0.8, 0.55) * disc
  output color = mix(sky, dusk.rgb * 0.5, saturate(-skyDir.y * 4))
}
//@layout { base: (-912, 0), cirrus: (-912, 278), disc: (-912, 812), in.skyDir: (-4332, 0), in.time: (-3420, 256), mask: (-912, 556), out: (0, 0), ribbons: (-1824, 512), sky: (-456, 0), sp: (-3420, 0), sun: (-2052, 0), toSun: (-1596, 672), u.band: (-1596, 146), u.dusk: (-1596, 526), u.glow: (-1596, 0), u.scroll: (-3192, 512), u.streaks: (-1596, 1184), u.sunDir: (-2280, 0), up: (-1596, 292) }
"#;

const STORM_NIGHT: &str = r#"// Storm night — a churning storm deck with real lightning. Each strike picks
// a random moment AND a random direction (valueNoise seeded by the cycle
// number is the poor man's random), double-flashes, and lights the THIN
// parts of the clouds from behind — the way a real storm silhouettes itself.
shader stormNight {
  stage sky
  uniform cloudDark: color = #07090F
  uniform cloudLit: color = #3D4A66
  uniform horizonGlow: color = #232E44
  uniform flash: color = #DCE6FF
  uniform boil: float = 1 range(0, 4)
  uniform strikes: float = 1 range(0, 3)

  let up = saturate(skyDir.y)
  let sp = skyDir.xz / (max(skyDir.y, 0.0) * 1.8 + 0.28)
  let scrolled = vec2(time * 0.055 * boil, time * 0.024 * boil)
  let churned = domainWarp(sp * 1.6 + scrolled, scale: 1.6, time: time * 0.11 * boil, strength: 1.8)
  let deck = fbm(churned, octaves: 6) * 0.5 + 0.5
  let towers = fbm(sp * 3.6 - scrolled * 1.7, octaves: 4) * 0.5 + 0.5
  let hfade = smoothstep(0.01, 0.16, skyDir.y)
  let dens = mix(0.62, saturate(deck * 1.15 + towers * 0.4 - 0.12), hfade)
  let body = mix(cloudLit.rgb, cloudDark.rgb, smoothstep(0.1, 0.85, dens))
  let cycle = floor(time * 2.2 * strikes)
  let ph = fract(time * 2.2 * strikes)
  let strike = step(0.3, valueNoise(vec2(cycle * 0.719, 4.7)))
  let burst = pow(saturate(1 - ph * 6), 2) + 0.8 * pow(saturate(1 - abs(ph - 0.18) * 9), 2)
  let fx = valueNoise(vec2(cycle * 0.531, 1.7)) * 2 - 1
  let fz = valueNoise(vec2(cycle * 0.877, 9.2)) * 2 - 1
  let sector = pow(saturate(dot(skyDir, normalize(vec3(fx, 0.25, fz)))), 3)
  let lightning = flash.rgb * strike * burst * (sector * 1.3 + 0.15) * (0.2 + 1.1 * pow(1 - dens, 2))
  let sky = body + horizonGlow.rgb * pow(1 - up, 3) + lightning
  output color = mix(sky, cloudDark.rgb * 0.7, saturate(-skyDir.y * 3))
}
//@layout { body: (-912, 0), burst: (-1368, 950), churned: (-3648, 0), cycle: (-3876, 1024), deck: (-2964, 0), dens: (-1824, 0), fx: (-2736, 746), fz: (-2736, 1002), hfade: (-2052, 234), in.skyDir: (-5472, 0), in.time: (-4788, 256), lightning: (-684, 256), out: (0, 0), ph: (-3192, 512), scrolled: (-4104, 256), sector: (-1596, 1258), sky: (-456, 0), sp: (-4332, 0), strike: (-1596, 490), towers: (-2964, 256), u.boil: (-4560, 768), u.cloudDark: (-1368, 146), u.cloudLit: (-1368, 0), u.flash: (-1824, 534), u.horizonGlow: (-1368, 292), u.strikes: (-4332, 1024), up: (-1596, 0) }
"#;

const STARRY_NIGHT: &str = r#"// Starry night — a worley star field wheeling slowly overhead (the night
// turns!) under a milky way. Worley puts ONE feature point in each unit cell,
// so `floor(p)` is a stable per-star id: feed it to valueNoise and you get a
// per-star random that culls, sizes and twinkles that star independently.
shader starryNight {
  stage sky
  uniform skyLow: color = #101830
  uniform skyHigh: color = #04060D
  uniform density: float = 0.65 range(0, 1)
  uniform twinkle: float = 1 range(0, 4)
  uniform spin: float = 1 range(0, 5)
  uniform milkyWay: float = 0.8 range(0, 2)
  uniform bandPole: vec3 = vec3(0.5, 0.3, 0.8)

  let up = saturate(skyDir.y)
  let base = mix(skyLow.rgb, skyHigh.rgb, pow(up, 0.6))
  let ca = cos(time * 0.008 * spin)
  let sa = sin(time * 0.008 * spin)
  let turned = vec3(skyDir.x * ca - skyDir.z * sa, skyDir.y, skyDir.x * sa + skyDir.z * ca)
  let sp = turned * 34
  let rnd = valueNoise(floor(sp) + 0.5)
  let core = 1 - smoothstep(0.03, 0.14, worley(sp))
  let blink = 0.55 + 0.45 * sin(time * (1 + rnd * 2.5) * twinkle + rnd * 40)
  let stars = core * step(1 - density * 0.75, rnd) * (0.4 + rnd) * blink * 1.5
  let offAxis = dot(turned, normalize(bandPole))
  let bandMask = exp(offAxis * offAxis * -22)
  let dust = fbm(turned * 5, octaves: 5) * 0.5 + 0.5
  let dustColor = mix(vec3(0.3, 0.38, 0.55), vec3(0.7, 0.6, 0.65), dust)
  let wisp = bandMask * milkyWay * (dust * dust * 0.45 + 0.06)
  let sky = base + vec3(0.85, 0.92, 1) * stars + dustColor * wisp + skyLow.rgb * pow(1 - up, 4) * 0.35
  output color = mix(sky, skyHigh.rgb * 0.6, saturate(-skyDir.y * 3))
}
//@layout { bandMask: (-1596, 782), base: (-1140, 0), blink: (-1824, 512), ca: (-5244, 256), core: (-2280, 0), dust: (-2052, 768), dustColor: (-1140, 534), in.skyDir: (-5472, 0), in.time: (-5928, 0), offAxis: (-2280, 1002), out: (0, 0), rnd: (-3648, 0), sa: (-5244, 746), sky: (-456, 0), sp: (-4332, 0), stars: (-1368, 768), turned: (-4560, 0), u.bandPole: (-2736, 1258), u.density: (-2964, 0), u.milkyWay: (-1596, 1016), u.skyHigh: (-1596, 146), u.skyLow: (-1596, 0), u.spin: (-5700, 256), u.twinkle: (-2964, 402), up: (-1596, 292), wisp: (-1140, 812) }
"#;

const MOONLIT_CLOUDS: &str = r#"// Moonlit clouds — a night deck sliding under a hard little moon. Clouds
// facing the moon catch a silver lining, the disc dims behind them instead
// of vanishing, and a few twinkling stars survive in the gaps.
shader moonlitClouds {
  stage sky
  uniform night: color = #0A1124
  uniform haze: color = #1B2740
  uniform moonGlow: color = #E9EFFF
  uniform moonDir: vec3 = vec3(0.4, 0.55, 0.3)
  uniform cover: float = 0.55 range(0, 1)
  uniform drift: float = 1 range(0, 4)

  let up = saturate(skyDir.y)
  let base = mix(haze.rgb, night.rgb, pow(up, 0.8))
  let moon = normalize(moonDir)
  let toMoon = saturate(dot(skyDir, moon))
  let disc = smoothstep(0.9975, 0.9986, toMoon) * 1.6
  let halo = pow(toMoon, 90) * 0.5 + pow(toMoon, 12) * 0.12
  let rnd = valueNoise(floor(skyDir * 30) + 0.5)
  let stars = (1 - smoothstep(0.02, 0.09, worley(skyDir * 30))) * step(0.7, rnd) * (0.5 + 0.5 * sin(time * 2 + rnd * 30)) * 1.2
  let sp = skyDir.xz / (max(skyDir.y, 0.0) * 2.2 + 0.3)
  let scrolled = vec2(time * 0.016 * drift, time * 0.009 * drift)
  let puffs = fbm(sp * 3 + scrolled, octaves: 5) * 0.5 + 0.5
  let detail = fbm(sp * 9 - scrolled * 2, octaves: 4) * 0.5 + 0.5
  let shape = puffs * 0.85 + detail * 0.25
  let dens = smoothstep(0.95 - cover * 0.9, 1.12 - cover * 0.9, shape) * smoothstep(0.015, 0.12, skyDir.y)
  let lining = mix(night.rgb * 1.35, moonGlow.rgb * 0.95, pow(toMoon, 4) * 0.85 + saturate(detail * 1.2 - 0.5) * 0.12)
  let open = 1 - dens
  let sky = base + stars * open * vec3(0.8, 0.88, 1) + (disc + halo) * moonGlow.rgb * (open * 0.92 + 0.08)
  output color = mix(mix(sky, lining, dens), haze.rgb * 0.55, saturate(-skyDir.y * 3))
}
//@layout { base: (-1140, 0), dens: (-1824, 512), detail: (-2736, 892), disc: (-1596, 1038), halo: (-1596, 1294), in.skyDir: (-5016, 0), in.time: (-4560, 0), lining: (-684, 256), moon: (-2736, 1148), open: (-1596, 782), out: (0, 0), puffs: (-2736, 636), rnd: (-3192, 0), scrolled: (-3876, 512), shape: (-2280, 1280), sky: (-684, 0), sp: (-3876, 256), stars: (-1596, 526), toMoon: (-2280, 1792), u.cover: (-2736, 490), u.drift: (-4332, 512), u.haze: (-1596, 0), u.moonDir: (-2964, 1280), u.moonGlow: (-1596, 1550), u.night: (-1596, 146), up: (-1596, 292) }
"#;

const AURORA_VEIL: &str = r#"// Aurora veil — curtains of light where a warped fbm crosses zero (the
// pow(1 - |noise|) ridge trick), swaying sideways, shimmering along their
// height, and ramping green → violet with altitude. Stars keep the dark
// parts of the sky alive.
shader auroraVeil {
  stage sky
  uniform low: color = #37FF9E
  uniform high: color = #7C4DFF
  uniform night: color = #060B18
  uniform intensity: float = 1.3 range(0, 3)
  uniform sway: float = 1 range(0, 4)

  let up = saturate(skyDir.y)
  let base = mix(night.rgb * 2, night.rgb * 0.55, up)
  let rnd = valueNoise(floor(skyDir * 30) + 0.5)
  let stars = (1 - smoothstep(0.02, 0.08, worley(skyDir * 30))) * step(0.68, rnd) * (0.5 + 0.5 * sin(time * 1.5 + rnd * 25))
  let across = vec2(skyDir.x, skyDir.z) * 2.1
  let waved = domainWarp(across + vec2(time * 0.03 * sway, 0), scale: 1.3, time: time * 0.06 * sway, strength: 1.7)
  let ridge = fbm(waved, octaves: 4)
  let curtain = pow(saturate(1 - abs(ridge) * 2.9), 4)
  let shimmer = 0.7 + 0.3 * sin(skyDir.y * 34 - time * 2.2 * sway + ridge * 9)
  let heights = smoothstep(0.02, 0.16, up) * (1 - smoothstep(0.34, 0.62, up))
  let ramp = mix(low.rgb, high.rgb, saturate((up - 0.02) * 3))
  let glow = ramp * curtain * shimmer * heights * intensity * 1.5
  let sky = base + stars * vec3(0.8, 0.9, 1) + glow + low.rgb * pow(1 - up, 6) * curtain * 0.12
  output color = mix(sky, night.rgb * 0.7, saturate(-skyDir.y * 3))
}
//@layout { across: (-3876, 0), base: (-1140, 0), curtain: (-2052, 790), glow: (-912, 256), heights: (-1596, 1280), in.skyDir: (-4560, 0), in.time: (-4560, 212), out: (0, 0), ramp: (-2052, 512), ridge: (-3192, 490), rnd: (-2736, 0), shimmer: (-1824, 1170), sky: (-456, 0), stars: (-1368, 512), u.high: (-2508, 914), u.intensity: (-1368, 1024), u.low: (-2508, 768), u.night: (-1824, 0), u.sway: (-4332, 768), up: (-2964, 0), waved: (-3420, 256) }
"#;

const RETRO_SUN: &str = r#"// Retro sun — the synthwave poster: a posterized dusk, a giant striped sun
// (the stripes scroll down it, and the gaps widen toward the horizon), and a
// neon grid floor rolling toward you forever. Pure skyDir math, no texture.
shader retroSun {
  stage sky
  uniform top: color = #1B0637
  uniform bandColor: color = #FF2E97
  uniform sunTop: color = #FFD319
  uniform sunLow: color = #FF2E97
  uniform grid: color = #2DE2E6
  uniform sunDir: vec3 = vec3(0, 0.16, 1)
  uniform scroll: float = 1 range(0, 4)

  let up = saturate(skyDir.y)
  let base = posterize(mix(bandColor.rgb * 0.5, top.rgb, smoothstep(0.0, 0.55, up)), steps: 7)
  let sun = normalize(sunDir)
  let d = distance(skyDir, sun)
  let disc = 1 - smoothstep(0.235, 0.245, d)
  let slice = step(mix(-1.2, 0.85, saturate((sun.y - skyDir.y) * 4 + 0.12)), sin((skyDir.y - sun.y) * 110 + time * 2.5 * scroll))
  let sunBody = mix(sunLow.rgb, sunTop.rgb, saturate((skyDir.y - sun.y) * 3.5 + 0.55))
  let glowAmt = pow(saturate(1 - d * 1.4), 3) * 0.55
  let below = saturate(-skyDir.y * 50)
  let gp = skyDir.xz / min(skyDir.y, -0.02) * 3 + vec2(0, time * 1.2 * scroll)
  let lineX = smoothstep(0.9, 0.985, abs(fract(gp.x) * 2 - 1))
  let lineZ = smoothstep(0.9, 0.985, abs(fract(gp.y) * 2 - 1))
  let gfade = smoothstep(0.03, 0.3, -skyDir.y)
  let floorCol = top.rgb * 0.35 + grid.rgb * max(lineX, lineZ) * gfade + bandColor.rgb * pow(1 - gfade, 8) * 0.6
  let skyCol = mix(base + bandColor.rgb * glowAmt, sunBody, disc * slice)
  output color = mix(skyCol, floorCol, below)
}
//@layout { base: (-912, 0), below: (-456, 534), d: (-2280, 0), disc: (-912, 1258), floorCol: (-456, 278), gfade: (-1596, 2596), glowAmt: (-1140, 534), gp: (-2964, 0), in.skyDir: (-4104, 0), in.time: (-3876, 0), lineX: (-1596, 2040), lineZ: (-1596, 2318), out: (0, 0), skyCol: (-456, 0), slice: (-912, 1514), sun: (-2508, 0), sunBody: (-684, 256), u.bandColor: (-1824, 0), u.grid: (-1596, 1894), u.scroll: (-3648, 0), u.sunDir: (-2736, 0), u.sunLow: (-1140, 790), u.sunTop: (-1140, 936), u.top: (-1596, 256), up: (-1596, 402) }
"#;

const NEBULA_DREAM: &str = r#"// Nebula dream — you're floating INSIDE the cloud: domain-warped fbm swirls
// in slow motion, a cosine palette paints it, and the whole sky drifts around
// the hue wheel over a couple of minutes. Stars burn through where it thins.
shader nebulaDream {
  stage sky
  uniform deep: color = #050310
  uniform density: float = 1 range(0.2, 2.5)
  uniform swirl: float = 1 range(0, 3)
  uniform hueDrift: float = 0.5 range(0, 2)

  let p = domainWarp(skyDir * 1.8, scale: 1.5, time: time * 0.04 * swirl, strength: 2.3)
  let body = fbm(p, octaves: 6) * 0.5 + 0.5
  let veil = fbm(p * 2.3 + vec3(0, time * 0.012 * swirl, 0), octaves: 4) * 0.5 + 0.5
  let neb = pow(body, 3) * 1.45 * density
  let paint = contrast(hueShift(palette(body * 0.9 + veil * 0.35, "bruise"), time * 0.01 * hueDrift), 1.3)
  let cores = pow(saturate(veil * body * 2 - 0.35), 2) * 0.55
  let rnd = valueNoise(floor(skyDir * 44) + 0.5)
  let stars = (1 - smoothstep(0.02, 0.07, worley(skyDir * 44))) * step(0.5, rnd) * (0.45 + rnd) * 1.3
  let blink = 0.7 + 0.3 * sin(time * 3 + rnd * 30)
  output color = deep.rgb + paint * neb + paint * cores + vec3(0.9, 0.9, 1) * stars * blink * saturate(1.2 - neb * 1.8)
}
//@layout { blink: (-912, 1024), body: (-2280, 0), cores: (-912, 512), in.skyDir: (-3876, 0), in.time: (-4104, 0), neb: (-1368, 256), out: (0, 0), p: (-3420, 0), paint: (-1140, 146), rnd: (-2052, 1046), stars: (-1140, 658), u.deep: (-1140, 0), u.density: (-1596, 768), u.hueDrift: (-1824, 512), u.swirl: (-3876, 468), veil: (-2280, 256) }
"#;

const WOBBLE_ORB: &str = r#"// Wobble orb — this shader IS geometry: assign it to a ◈ Field Shape node
// (Add → Field Shape) and the scene raymarches it, shadows and all. Space is
// twisted over time, then a sphere and a rounded box melt together.
shader wobbleOrb {
  stage sdf
  uniform wobble: float = 0.6 range(0, 3)
  uniform blend: float = 0.35 range(0.05, 1)

  let p = twist(worldPos, wobble * sin(time * 0.7))
  let body = smoothMin(sphere(p, radius: 0.75), box(p, vec3(0.55, 0.35, 0.55), rounding: 0.08), k: blend)
  let swirl = noise(worldPos * 2.5 + time * 0.3) * 0.5 + 0.5

  output sdf = body
  output color = mix(vec3(0.55, 0.2, 0.75), vec3(0.15, 0.9, 0.8), swirl)
}
//@layout { body: (-228, 0), in.time: (-1596, 212), in.worldPos: (-1596, 0), out: (0, 0), p: (-684, 0), swirl: (-456, 680), u.blend: (-456, 534), u.wobble: (-1140, 0) }
"#;

const RING_TOWER: &str = r#"// Ring tower — repeat() tiles space vertically, so ONE torus becomes an
// endless stack; intersecting with a sphere keeps it inside the node's
// bounds. Move/rotate/scale the Field Shape node like anything else.
shader ringTower {
  stage sdf
  uniform spacing: float = 0.55 range(0.2, 2)
  uniform major: float = 0.6 range(0.1, 1.5)
  uniform minor: float = 0.12 range(0.02, 0.5)

  let stack = repeat(worldPos, vec3(0, spacing, 0))
  let rings = torus(stack, major: major, minor: minor)
  let bound = sphere(worldPos, radius: 1)
  let solid = opIntersect(rings, bound)
  let stripes = fract(worldPos.y / spacing + 0.5)

  output sdf = solid
  output color = mix(vec3(0.9, 0.6, 0.2), vec3(0.3, 0.2, 0.5), stripes)
}
//@layout { bound: (-456, 278), in.worldPos: (-1368, 0), out: (0, 0), rings: (-456, 0), solid: (-228, 0), stack: (-684, 0), stripes: (-456, 534), u.major: (-684, 256), u.minor: (-684, 402), u.spacing: (-1140, 0) }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Stage;

    /// Every example compiles for its stage — fragment ones all the way
    /// through naga against the transpiler's prelude mirror, sdf ones through
    /// parse/check/per-slot transpile (the splice itself is naga-gated at
    /// runtime and pixel-covered by `field_shape_probe`).
    #[test]
    fn every_example_compiles() {
        for (name, src) in EXAMPLES {
            let ir = crate::text::parse(src).unwrap_or_else(|e| {
                let (l, c) = crate::text::line_col(src, e.span.start);
                panic!("{name} parse {l}:{c}: {}", e.message)
            });
            match ir.stage {
                Some(Stage::Fragment) => {
                    let compiled = crate::compile_fragment(src)
                        .unwrap_or_else(|e| panic!("{name}: {e}"));
                    crate::transpile::validate(crate::transpile::TEST_PRELUDE, &compiled.chunk)
                        .unwrap_or_else(|e| panic!("{name}: naga rejects: {}", e.message));
                }
                Some(Stage::Sdf) => {
                    let (ir, ck) =
                        crate::check_sdf(src).unwrap_or_else(|e| panic!("{name}: {e}"));
                    crate::transpile::transpile_sdf(&ir, &ck, 0)
                        .unwrap_or_else(|e| panic!("{name}: {}", e.message));
                }
                Some(Stage::Sky) => {
                    crate::compile_sky(src).unwrap_or_else(|e| panic!("{name}: {e}"));
                }
                Some(Stage::Ui) => {
                    crate::compile_ui(src).unwrap_or_else(|e| panic!("{name}: {e}"));
                }
                None => panic!("{name}: missing stage"),
            }
        }
    }

    /// The graph view opens every example without panicking, and the hand
    /// `//@layout` names all refer to real nodes.
    #[test]
    fn every_example_builds_a_view() {
        for (name, src) in EXAMPLES {
            let ir = crate::text::parse(src).unwrap_or_else(|e| panic!("{name}: {}", e.message));
            let ck = crate::ir::check(&ir).unwrap_or_else(|e| panic!("{name}: {:?}", e));
            let view = crate::graph::build_view(&ir, Some(&ck));
            assert!(view.len() > 3, "{name}: view has nodes");
            for key in ir.layout.keys() {
                let hit = view
                    .iter()
                    .any(|n| n.key.layout_key().as_deref() == Some(key.as_str()));
                assert!(hit, "{name}: layout key `{key}` names no node");
            }
        }
    }
}
