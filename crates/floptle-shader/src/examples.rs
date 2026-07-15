//! The built-in example shaders — seeded into every project's
//! `shaders/examples/` folder (delete the folder and it stays deleted).
//!
//! They're teaching material first: each one is a worked example of a
//! different corner of the system (uniform knobs, blends, engine hooks, the
//! sdf stage), commented for someone who has never written a shader, and laid
//! out to open readably in the graph editor. Every entry is compile-tested.

/// `(file name, source)` for every example. Fragment ones go on mesh
/// Materials; sdf ones on ◈ Field Shape nodes.
pub const EXAMPLES: &[(&str, &str)] = &[
    ("plasma.flsl", PLASMA),
    ("water.flsl", WATER),
    ("lavaFlow.flsl", LAVA_FLOW),
    ("toonPrint.flsl", TOON_PRINT),
    ("hologram.flsl", HOLOGRAM),
    ("dissolve.flsl", DISSOLVE),
    ("forceField.flsl", FORCE_FIELD),
    ("fieldGlow.flsl", FIELD_GLOW),
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
