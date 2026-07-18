//! Generate the FULL Floptle Solar system (S7) from one master seed: a Sun
//! (visual + gravity root), three planets and two moons — every body real
//! diggable terrain with its own character, all on Kepler rails around the
//! star, lit by the POSITIONAL sun (radial terminators + shadows), far bodies
//! drawn as impostors by the editor.
//!
//! Writes, for scene "system":
//!   <out_dir>/system.<id>.cfield   (terrain ids 1..=5)
//!   <out_dir>/system.palette       (shared 12-slot palette, |glow suffixes)
//!   solar/scenes/system.ron        (the playable scene: bodies + ship + HUD)
//!
//! Body variety rides per-voxel TINTS over the shared palette (albedo =
//! texture × tint), so five bodies share one 12-slot atlas without looking
//! alike: rust canyon world, golden dune world, pale ice world, grey moon,
//! briny frost moon.
//!
//! Usage: cargo run --release -p floptle-field --example gen_solar [-- <out_dir> [seed]]
//! Default out_dir `solar/terrain`, seed 11.

use floptle_core::math::Vec3;
use floptle_core::noise::Noise;
use floptle_field::ChunkField;

const PALETTE: [(&str, bool); 12] = [
    ("planet_rust_rock.png", false),      // 1  planet highlands
    ("planet_purple_dirt.png", false),    // 2  planet lowlands
    ("planet_purple_slate.png", false),   // 3  shallow strata
    ("planet_banded_onyx.png", false),    // 4  deep strata
    ("cave_orange_veinstone.png", false), // 5  cave-zone wall rock
    ("cave_magma_veins.png", true),       // 6  magma seams (GLOWS)
    ("cave_amethyst.png", true),          // 7  amethyst pockets (GLOWS)
    ("cave_blue_crystal.png", true),      // 8  blue crystal pockets (GLOWS)
    ("moon_silver_slate.png", false),     // 9  moon/ice subsurface
    ("moon_gray_regolith.png", false),    // 10 moon surface
    ("moon_crater_dust.png", false),      // 11 crater floors
    ("moon_teal_ice.png", false),         // 12 ice caps / frost
];

fn tint(base: [f32; 3], vary: f32) -> [u8; 3] {
    let v = 0.85 + 0.25 * vary.clamp(-1.0, 1.0);
    let f = |c: f32| (((c * v).clamp(0.0, 1.0) * 255.0 / 12.0).round() * 12.0) as u8;
    [f(base[0]), f(base[1]), f(base[2])]
}

fn rgba(rgb: [u8; 3], slot: u8) -> [u8; 4] {
    [rgb[0], rgb[1], rgb[2], slot]
}

/// One body's look: which palette slots it wears and with which tints.
#[derive(Clone, Copy)]
enum Style {
    Rust,
    Dune,
    Ice,
    Moon,
    FrostMoon,
}

struct BodySpec {
    name: &'static str,
    id: u32,
    radius: f32,
    voxel: f32,
    relief: f32,
    /// Cave zone thickness below the surface (0 = solid body).
    cave_depth: f32,
    style: Style,
    seed_salt: u32,
}

fn gen_body(spec: &BodySpec, master_seed: u32) -> ChunkField {
    let noise = Noise::new(master_seed.wrapping_mul(97).wrapping_add(spec.seed_salt));
    let mut field = ChunkField::new(spec.voxel);
    let ext = spec.radius + spec.relief + 2.0;
    let (radius, relief, cave_depth, style) =
        (spec.radius, spec.relief, spec.cave_depth, spec.style);
    // Dune worlds ripple at a higher frequency (dune fields), ice worlds roll.
    let bump_freq = match style {
        Style::Dune => 7.5,
        Style::Ice => 3.2,
        _ => 4.3,
    };
    field.fill_with_rgba(
        Vec3::splat(-ext),
        Vec3::splat(ext),
        |p| {
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let bump = noise.fbm(dir * bump_freq, 5) * relief;
            let body = r - (radius + bump);
            if cave_depth <= 0.0 {
                return body;
            }
            // Tunnels where two noise fields are both near zero, ≥3 voxels of
            // crust kept, confined to the outer cave zone (chunk residency).
            let a = noise.fbm(p * 0.018, 3);
            let b = noise.fbm(p * 0.018 + Vec3::splat(51.7), 3);
            let tunnel = (a.abs().max(b.abs()) - 0.1) * 34.0;
            let gated = tunnel.max(spec.voxel * 3.0 + body).max(-(body + cave_depth));
            body.max(-gated)
        },
        |p| {
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let bump = noise.fbm(dir * bump_freq, 5) * relief;
            let alt = (r - radius) / relief.max(1.0);
            let depth = (radius + bump) - r;
            let vary = noise.fbm(p * 0.05 + Vec3::splat(83.0), 2);
            let patch = noise.fbm(dir * 9.0 + Vec3::splat(37.0), 3);
            let pocket = noise.fbm(p * 0.05 + Vec3::splat(17.3), 3);
            match style {
                Style::Rust => {
                    if depth > 8.0 && pocket > 0.38 {
                        return rgba(tint([0.72, 0.65, 0.85], vary), 7);
                    }
                    if depth > 26.0 && noise.fbm(p * 0.035 + Vec3::splat(5.9), 3) > 0.12 {
                        return rgba(tint([0.95, 0.85, 0.72], vary), 6);
                    }
                    if depth > 22.0 {
                        return rgba(tint([0.78, 0.68, 0.58], vary), 5);
                    }
                    if depth > 9.0 {
                        return rgba(tint([0.85, 0.78, 0.66], vary), 4);
                    }
                    if depth > 2.2 {
                        return rgba(tint([0.6, 0.58, 0.68], vary), 3);
                    }
                    if patch + alt * 0.45 > 0.08 {
                        rgba(tint([0.72, 0.62, 0.58], vary), 1)
                    } else {
                        rgba(tint([0.66, 0.6, 0.74], vary), 2)
                    }
                }
                Style::Dune => {
                    // Gold dunes over red bedrock; magma runs deep under the sand.
                    if depth > 24.0 && noise.fbm(p * 0.03 + Vec3::splat(3.3), 3) > 0.18 {
                        return rgba(tint([1.0, 0.8, 0.6], vary), 6);
                    }
                    if depth > 18.0 {
                        return rgba(tint([0.9, 0.62, 0.45], vary), 5);
                    }
                    if depth > 6.0 {
                        return rgba(tint([0.95, 0.7, 0.5], vary), 4);
                    }
                    if patch + alt * 0.6 > -0.05 {
                        rgba(tint([0.98, 0.85, 0.55], vary), 2)
                    } else {
                        rgba(tint([0.9, 0.7, 0.5], vary), 1)
                    }
                }
                Style::Ice => {
                    // Pale ice crust over blue slate; shallow blue-crystal pockets
                    // glow through dig walls.
                    if depth > 6.0 && pocket > 0.34 {
                        return rgba(tint([0.68, 0.78, 0.95], vary), 8);
                    }
                    if depth > 4.0 {
                        return rgba(tint([0.62, 0.7, 0.82], vary), 9);
                    }
                    if patch > 0.25 {
                        rgba(tint([0.72, 0.78, 0.85], vary), 9)
                    } else {
                        rgba(tint([0.85, 0.92, 0.98], vary), 12)
                    }
                }
                Style::Moon => {
                    if depth > 3.0 && pocket > 0.45 {
                        return rgba(tint([0.68, 0.78, 0.95], vary), 8);
                    }
                    if depth > 2.5 {
                        return rgba(tint([0.6, 0.62, 0.68], vary), 9);
                    }
                    if patch < -0.42 {
                        rgba(tint([0.78, 0.74, 0.66], vary), 11)
                    } else {
                        rgba(tint([0.66, 0.66, 0.68], vary), 10)
                    }
                }
                Style::FrostMoon => {
                    if depth > 3.0 && pocket > 0.4 {
                        return rgba(tint([0.68, 0.78, 0.95], vary), 8);
                    }
                    if depth > 2.5 {
                        return rgba(tint([0.62, 0.7, 0.82], vary), 9);
                    }
                    if dir.y.abs() > 0.6 || patch < -0.2 {
                        rgba(tint([0.8, 0.9, 0.95], vary), 12)
                    } else {
                        rgba(tint([0.7, 0.72, 0.75], vary), 10)
                    }
                }
            }
        },
    );
    field
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_dir = args.get(1).map(String::as_str).unwrap_or("solar/terrain");
    let seed: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(11);

    let bodies = [
        BodySpec { name: "Rustholm", id: 1, radius: 150.0, voxel: 1.5, relief: 10.0, cave_depth: 40.0, style: Style::Rust, seed_salt: 1 },
        BodySpec { name: "Duneveil", id: 2, radius: 200.0, voxel: 2.0, relief: 16.0, cave_depth: 34.0, style: Style::Dune, seed_salt: 2 },
        BodySpec { name: "Palegate", id: 3, radius: 120.0, voxel: 1.5, relief: 8.0, cave_depth: 26.0, style: Style::Ice, seed_salt: 3 },
        BodySpec { name: "Ashmoor", id: 4, radius: 36.0, voxel: 1.0, relief: 3.0, cave_depth: 12.0, style: Style::Moon, seed_salt: 4 },
        BodySpec { name: "Brine", id: 5, radius: 44.0, voxel: 1.0, relief: 4.0, cave_depth: 14.0, style: Style::FrostMoon, seed_salt: 5 },
    ];

    std::fs::create_dir_all(out_dir).expect("create out dir");
    for spec in &bodies {
        let t0 = std::time::Instant::now();
        let field = gen_body(spec, seed);
        let bytes = field.to_bytes();
        let (worst, frac) = field.lipschitz_audit();
        let path = format!("{out_dir}/system.{}.cfield", spec.id);
        std::fs::write(&path, &bytes).expect("write cfield");
        println!(
            "{:9} r {:5.0}: {} chunks, {:5.1} MB disk, fill {} ms, |grad| worst {worst:.2} ({:.1}% over) → {path}",
            spec.name,
            spec.radius,
            field.data_chunks(),
            bytes.len() as f64 / 1e6,
            t0.elapsed().as_millis(),
            frac * 100.0,
        );
    }

    // Palette sidecar (absolute paths — asset paths resolve as-is from CWD).
    let tex_dir = std::path::Path::new(out_dir)
        .parent()
        .map(|p| p.join("textures/terrain"))
        .unwrap_or_else(|| "solar/textures/terrain".into());
    let lines: Vec<String> = PALETTE
        .iter()
        .map(|(name, glow)| {
            let p = tex_dir.join(name);
            let abs = std::fs::canonicalize(&p).unwrap_or(p);
            format!("{}{}", abs.display(), if *glow { "|glow" } else { "" })
        })
        .collect();
    let ppath = format!("{out_dir}/system.palette");
    std::fs::write(&ppath, lines.join("\n")).expect("write palette");
    println!("palette   12 slots (3 glowing) → {ppath}");

    // ---- The scene. Sun µ 6e7: Rustholm's year ≈ 6.3 min at a=6000 (planets
    // visibly move), SOIs stay separated (A ≈ 590 ⊃ moon at 420; B ≈ 1550 ⊃
    // moon at 700; C ≈ 1600). Player + ship spawn at Rustholm's north pole.
    let scene = build_scene();
    std::fs::write("solar/scenes/system.ron", &scene).expect("write scene");
    println!("scene     solar/scenes/system.ron ({} bytes)", scene.len());
    println!("\nOpen scene \"system\" in the solar project and press Play.");
}

fn build_scene() -> String {
    let mut nodes = String::new();
    let node = |body: &str| -> String { body.to_string() };

    // 0 — the Sun: gravity root + a big unlit emissive sphere (bloom halo).
    nodes.push_str(&node(r#"        (
            name: "Sun",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (941.0, 941.0, 941.0),
            ),
            matter: Primitive(
                shape: Sphere,
                color: (1.0, 0.92, 0.6),
            ),
            scripts: [],
            material: Some((
                color: (1.0, 0.92, 0.6),
                emissive: (1.0, 0.85, 0.45),
                emissive_strength: 2.0,
                unlit: true,
            )),
            celestial: Some((
                mu: 60000000.0,
                body_radius: 800.0,
                soi: 0.0,
                a: 0.0, e: 0.0, i: 0.0, lan: 0.0, arg_pe: 0.0, m0: 0.0,
            )),
        ),
"#));
    // 1..5 — the terrain bodies on rails.
    // Atmosphere per body: (sky color, shell height, density); moons airless.
    let celestial = |name: &str, id: u32, pos: [f64; 3], mu: f64, radius: f64,
                         parent: &str, a: f64, i: f64, m0: f64,
                         atmo: Option<([f32; 3], f64, f32)>| -> String {
        let atmo_lines = match atmo {
            Some((c, h, d)) => format!(
                "\n                atmo_color: ({:.2}, {:.2}, {:.2}),\n                atmo_height: {h:.1},\n                atmo_density: {d:.2},",
                c[0], c[1], c[2]
            ),
            None => String::new(),
        };
        format!(
            r#"        (
            name: "{name}",
            transform: (
                translation: ({x:.1}, {y:.1}, {z:.1}),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Terrain(
                id: {id},
            ),
            scripts: [],
            celestial: Some((
                mu: {mu:.1},
                body_radius: {radius:.1},
                soi: 0.0,
                parent: "{parent}",
                a: {a:.1}, e: 0.0, i: {i}, lan: 0.0, arg_pe: 0.0, m0: {m0},{atmo_lines}
            )),
        ),
"#,
            x = pos[0], y = pos[1], z = pos[2],
        )
    };
    nodes.push_str(&celestial("Rustholm", 1, [6000.0, 0.0, 0.0], 180000.0, 150.0, "Sun", 6000.0, 0.0, 0.0, Some(([0.78, 0.42, 0.3], 90.0, 0.85))));
    nodes.push_str(&celestial("Duneveil", 2, [-6050.0, 0.0, 10380.0], 360000.0, 200.0, "Sun", 12000.0, 0.06, 2.1, Some(([0.93, 0.72, 0.42], 120.0, 0.9))));
    nodes.push_str(&celestial("Palegate", 3, [-6750.0, 0.0, -20940.0], 86400.0, 120.0, "Sun", 22000.0, 0.12, 4.4, Some(([0.5, 0.78, 0.9], 60.0, 0.55))));
    nodes.push_str(&celestial("Ashmoor", 4, [6260.0, 0.0, 330.0], 5200.0, 36.0, "Rustholm", 420.0, 0.1, 0.9, None));
    nodes.push_str(&celestial("Brine", 5, [-5350.0, 0.0, 10380.0], 8700.0, 44.0, "Duneveil", 700.0, 0.05, 0.0, None));

    // 6 skybox, 7 post (indices fixed from here down — parent refs use them).
    nodes.push_str(&node(r#"        (
            name: "Skybox",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Skybox(
                color: (1.0, 1.0, 1.0),
                size: 250000.0,
                tint: (0.004, 0.005, 0.012),
            ),
            scripts: [],
        ),
        (
            name: "Post Processing",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: PostProcess(
                enabled: true,
                bloom: true,
                bloom_threshold: 0.3,
                bloom_intensity: 0.3,
                vignette: true,
                vignette_strength: 0.4,
                vignette_radius: 0.5,
                ao: Off,
                ao_strength: 1.0,
                ao_radius: 0.7,
                posterize_bands: 0,
                posterize_dither: false,
            ),
            scripts: [],
        ),
"#));
    // 8 astronaut, 9 camera — spawn at Rustholm's north pole.
    nodes.push_str(&node(r#"        (
            name: "Astronaut",
            transform: (
                translation: (6000.0, 175.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Primitive(
                shape: Capsule,
                color: (0.85, 0.6, 0.25),
            ),
            scripts: [
                (
                    kind: "planet_walker",
                    enabled: true,
                    params: [
                        ("double_tap", 0.3),
                        ("ground_ray", 1.6),
                        ("jump", 10.4),
                        ("run", 9.4),
                        ("walk", 4.5),
                    ],
                ),
                (
                    kind: "dig_tool",
                    enabled: true,
                    params: [
                        ("radius", 3.0),
                        ("range", 45.0),
                        ("rate", 0.12),
                        ("spacing", 0.45),
                        ("strength", 0.6),
                    ],
                ),
            ],
            rigidbody: Some((
                capsule: true,
                radius: 0.5,
                height: 2.0,
                half_extents: (0.5, 0.5, 0.5),
                restitution: 0.0,
                friction: 0.3,
                gravity: true,
                lock_pos: (false, false, false),
                lock_rot: (false, false, false),
                align_up: true,
            )),
        ),
        (
            name: "Camera 1",
            transform: (
                translation: (6000.0, 180.0, 8.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Camera(
                fov_y: 1.1519173,
                active: true,
            ),
            scripts: [
                (
                    kind: "planet_camera",
                    enabled: true,
                    params: [
                        ("distance", 7.0),
                        ("height", 1.2),
                        ("max_distance", 14.0),
                        ("min_distance", 1.5),
                        ("sensitivity", 0.3),
                        ("start_pitch", -0.3),
                        ("zoom_speed", 1.0),
                    ],
                ),
            ],
        ),
"#));
    // 10 ship (+11 flame), 12 HUD (+13 text, 14 navball, 15 map), 16..19 legs.
    nodes.push_str(&node(r#"        (
            name: "Ship",
            transform: (
                translation: (6020.0, 157.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.6, 2.4, 1.6),
            ),
            matter: Primitive(
                shape: Capsule,
                color: (0.85, 0.87, 0.92),
            ),
            scripts: [
                (
                    kind: "ship_controller",
                    enabled: true,
                    params: [],
                ),
            ],
            rigidbody: Some((
                capsule: false,
                radius: 2.0,
                height: 4.0,
                half_extents: (0.5, 0.5, 0.5),
                restitution: 0.0,
                friction: 0.05,
                gravity: true,
                lock_pos: (false, false, false),
                lock_rot: (false, false, false),
            )),
        ),
        (
            name: "Ship Flame",
            transform: (
                translation: (0.0, -1.0, 0.0),
                rotation: (1.0, 0.0, 0.0, 0.0),
                scale: (0.625, 0.41667, 0.625),
            ),
            matter: PointLight(
                color: (1.0, 0.62, 0.25),
                intensity: 0.0,
                range: 16.0,
            ),
            scripts: [],
            particles: Some((
                asset: "vfx/Flame",
                play_on_start: false,
            )),
            parent: Some(10),
        ),
        (
            name: "Ship HUD",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            ui_layer: Some((
                design_height: 720.0,
                z: 120,
                enabled: true,
                space: Screen,
                canvas_scale: 0.009,
            )),
        ),
        (
            name: "Ship HUD Text",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            parent: Some(12),
            ui: Some((
                place: Pin(
                    anchor: TopLeft,
                    offset: (16.0, 12.0),
                ),
                size: (Fixed(640.0), Fixed(200.0)),
                text: Some((
                    text: "",
                    size: 19.0,
                    color: (0.7, 1.0, 0.78, 1.0),
                    align: Start,
                    valign: Start,
                    fit: false,
                )),
                visible: false,
                opacity: 0.95,
            )),
        ),
        (
            name: "Navball",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            parent: Some(12),
            ui: Some((
                place: Pin(
                    anchor: Bottom,
                    offset: (0.0, -12.0),
                ),
                size: (Fixed(170.0), Fixed(170.0)),
                shader: "shaders/navball.flsl",
                visible: false,
                opacity: 0.95,
            )),
        ),
        (
            name: "Map",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            parent: Some(12),
            ui: Some((
                place: Pin(
                    anchor: Center,
                    offset: (0.0, 0.0),
                ),
                size: (Fixed(520.0), Fixed(520.0)),
                shader: "shaders/map.flsl",
                visible: false,
                opacity: 1.0,
            )),
        ),
"#));
    // 16..20 — the G5 instrument cluster (speed tape / alt tape / readouts / HDG).
    nodes.push_str(&node(r#"        (
            name: "Speed Tape",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            parent: Some(12),
            ui: Some((
                place: Pin(
                    anchor: Bottom,
                    offset: (-128.0, -22.0),
                ),
                size: (Fixed(52.0), Fixed(180.0)),
                shader: "shaders/tape.flsl",
                shader_params: {
                    "side": (0.0, 0.0, 0.0, 0.0),
                    "accent": (0.45, 0.95, 0.6, 1.0),
                },
                visible: false,
                opacity: 0.95,
            )),
        ),
        (
            name: "Alt Tape",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            parent: Some(12),
            ui: Some((
                place: Pin(
                    anchor: Bottom,
                    offset: (128.0, -22.0),
                ),
                size: (Fixed(52.0), Fixed(180.0)),
                shader: "shaders/tape.flsl",
                shader_params: {
                    "side": (1.0, 0.0, 0.0, 0.0),
                    "accent": (1.0, 0.75, 0.3, 1.0),
                },
                visible: false,
                opacity: 0.95,
            )),
        ),
        (
            name: "Speed Readout",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            parent: Some(12),
            ui: Some((
                place: Pin(
                    anchor: Bottom,
                    offset: (-185.0, -104.0),
                ),
                size: (Fixed(58.0), Fixed(24.0)),
                text: Some((
                    text: "0",
                    size: 17.0,
                    color: (0.45, 0.95, 0.6, 1.0),
                    align: End,
                    valign: Center,
                    fit: false,
                )),
                visible: false,
                opacity: 0.95,
            )),
        ),
        (
            name: "Alt Readout",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            parent: Some(12),
            ui: Some((
                place: Pin(
                    anchor: Bottom,
                    offset: (185.0, -104.0),
                ),
                size: (Fixed(58.0), Fixed(24.0)),
                text: Some((
                    text: "0",
                    size: 17.0,
                    color: (1.0, 0.75, 0.3, 1.0),
                    align: Start,
                    valign: Center,
                    fit: false,
                )),
                visible: false,
                opacity: 0.95,
            )),
        ),
        (
            name: "Heading Readout",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            parent: Some(12),
            ui: Some((
                place: Pin(
                    anchor: Bottom,
                    offset: (0.0, -188.0),
                ),
                size: (Fixed(120.0), Fixed(22.0)),
                text: Some((
                    text: "HDG 000°",
                    size: 16.0,
                    color: (0.85, 0.95, 1.0, 1.0),
                    align: Center,
                    valign: Center,
                    fit: false,
                )),
                visible: false,
                opacity: 0.95,
            )),
        ),
"#));
    // 21..24 — the landing legs.
    for (i, (x, z)) in [(0.52, 0.52), (-0.52, 0.52), (0.52, -0.52), (-0.52, -0.52)]
        .iter()
        .enumerate()
    {
        nodes.push_str(&format!(
            r#"        (
            name: "Leg {}",
            transform: (
                translation: ({x}, -0.6, {z}),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (0.1, 0.5, 0.1),
            ),
            matter: Primitive(
                shape: Cube,
                color: (0.38, 0.4, 0.46),
            ),
            scripts: [],
            parent: Some(10),
        ),
"#,
            ["A", "B", "C", "D"][i],
        ));
    }

    format!(
        r#"(
    name: "system",
    lighting: (
        direction: (0.4, 0.9, 0.45),
        positional: true,
        position: (0.0, 0.0, 0.0),
        color: (1.0, 0.98, 0.92),
        ambient: (0.1, 0.1, 0.14),
        intensity: 1.0,
        shadows: true,
        shadow_softness: 0.35,
        shadow_strength: 1.0,
        shadow_tint: (0.0, 0.0, 0.0),
        shadow_quantize: 0,
        shadow_dither: false,
        shadow_distance: 400.0,
    ),
    nodes: [
{nodes}    ],
)
"#
    )
}
