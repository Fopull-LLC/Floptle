use super::*;

/// The read list and the write list of a Material are one list
/// (that task's rule, applied here): a field the mirror publishes and
/// the applier ignores is a value a script can read, assign, and watch do
/// nothing.
///
/// It matters more since a per-object override is created by a write: the
/// write list is what decides whether a name is real enough to bring one
/// into being, so a name in one list and not the other either blanks a part
/// of a model on a typo or refuses a field that works everywhere else.
#[test]
fn every_material_field_can_be_both_read_and_written() {
    let m = floptle_core::Material::default();
    let readable = crate::api::material_fields_for_test(&m, 0);
    let writable: std::collections::HashSet<&str> =
        crate::api::MATERIAL_NUM_FIELDS.iter().copied().collect();
    let missing: Vec<&String> =
        readable.keys().filter(|k| !writable.contains(k.as_str())).collect();
    assert!(
        missing.is_empty(),
        "the mirror publishes {missing:?}, which no write can reach — a script can read \
         them, assign them and watch nothing happen"
    );
    // The other direction, minus the write-only spellings that are
    // aliases of a published field.
    let aliases = ["opacity"];
    let unreadable: Vec<&&str> = crate::api::MATERIAL_NUM_FIELDS
        .iter()
        .filter(|k| !readable.contains_key(**k) && !aliases.contains(k))
        .collect();
    assert!(unreadable.is_empty(), "writable but never readable: {unreadable:?}");
}

/// A tint is a multiplier over whatever a node already draws — the "same
/// model, but red" a Material cannot express, because a Material replaces.
///
/// Asked for as: *"there still needs to be an easy way to apply a tint to an
/// entire mesh without having to manually set everything."* One call, no
/// Material required, and the model keeps its own textures.
#[test]
fn a_script_tints_a_whole_model_without_replacing_anything() {
    use floptle_core::{Material, Matter, Tint};

    let dir = std::env::temp_dir().join(format!("floptle-tint-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "flash",
        concat!(
            "function start(node)\n",
            "  node:setTint(color(1, 0.3, 0.3))\n",
            "end\n",
            "function update(node, dt)\n",
            // …and the same value through the component route, which is
            // what an animation lane keys.
            "  local t = node:getcomponent('Tint')\n",
            "  log('alpha=' .. tostring(t and t.alpha or 'none'))\n",
            "  if clear then node:setTint() end\n",
            "end\n",
        ),
    );

    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Mesh { asset_path: "models/avatar.glb".into() });
    // A material with a texture, to prove the tint leaves it alone.
    world.insert(
        e,
        Material { texture: Some("art/skin.png".into()), ..Material::default() },
    );
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "flash".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );

    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let t = world.get::<Tint>(e).copied().expect("the tint landed");
    assert!((t.color[0] - 1.0).abs() < 1e-5 && (t.color[1] - 0.3).abs() < 1e-5);
    assert_eq!(t.alpha, 1.0, "no alpha given means opaque, not invisible");
    // The material it was wearing is untouched — that is the whole point.
    let m = world.get::<Material>(e).expect("still has its material");
    assert_eq!(m.texture.as_deref(), Some("art/skin.png"));
    assert_eq!(m.color, [1.0, 1.0, 1.0], "a tint is not a material write");

    // The component route sees it…
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert!(logs.iter().any(|l| l == "alpha=1.0" || l == "alpha=1"), "{logs:?}");

    // …and clearing puts the node back to carrying no tint at all, rather
    // than to carrying a white one nobody asked for.
    let e2 = world.spawn();
    world.insert(e2, Transform::IDENTITY);
    world.insert(e2, Tint { color: [1.0, 0.0, 0.0], alpha: 0.5, ..Default::default() });
    world.insert(
        e2,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "clearer".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    write_script(&dir, "clearer", "function start(node)\n  node:setTint()\nend\n");
    host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
    assert!(world.get::<Tint>(e2).is_none(), "setTint() with nothing clears it");
}

/// **A colour write must not cost the node its ambient lift.**
///
/// The lanes of a Tint are set at different times and by different code: a
/// character asks for its rim and its ambient once when it is dressed, and
/// rewrites its colour on every hit flash. If `setTint(red)` replaced the
/// whole component the character would drop back into the dark, untinted by
/// its rim, for exactly as long as the flash lasted — a bug that would read
/// as "the flash looks wrong" and never as "the merge is missing".
#[test]
fn a_colour_write_keeps_the_rim_and_the_ambient() {
    let dir = std::env::temp_dir().join(format!("floptle-tint2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Dressed once, then flashed — the order the fighter actually does it.
    write_script(
        &dir,
        "fighter",
        concat!(
            "function start(node)\n",
            "  node:setTint{ ambient = 1.6, rim = color(0.1, 0.4, 1.0), rimStrength = 1.3 }\n",
            "  node:setTint(color(0.92, 0.13, 0.15))\n",
            "end\n",
        ),
    );

    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Mesh { asset_path: "models/sae.glb".into() });
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "fighter".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);

    let t = world.get::<floptle_core::Tint>(e).copied().expect("the fighter is tinted");
    assert_eq!(t.color, [0.92, 0.13, 0.15], "the flash colour landed");
    // The three the second call never mentioned.
    assert_eq!(t.ambient, 1.6, "and the ambient lift survived it");
    assert_eq!(t.rim, [0.1, 0.4, 1.0], "and so did the rim colour");
    assert_eq!(t.rim_strength, 1.3, "and its strength");
}

/// **`setTint{ alpha = 0.5 }` must fade a model, not black it out.**
///
/// The table form is decided by name: a table carrying one of the option
/// keys is options, anything else is a colour. `alpha` is an option key —
/// the docs list it as one — and leaving it out of that test is not a
/// no-op. `read_color` defaults a missing r/g/b to zero, so
/// `{ alpha = 0.5 }` read as a colour is the colour black at full opacity:
/// the model goes dark and its fade never happens, with nothing logged.
///
/// That is the same failure that has now bitten this API twice — a table
/// the engine did not recognise as options, read as a colour, arriving as
/// black. The assertion is on the colour lane, because "did the alpha
/// land" alone would pass against a version that also blacked the model.
#[test]
fn an_alpha_only_tint_fades_the_model_instead_of_blacking_it() {
    let dir = std::env::temp_dir().join(format!("floptle-tint3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "fader",
        concat!(
            "function start(node)\n",
            "  node:setTint{ alpha = 0.5 }\n",
            "end\n",
        ),
    );

    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Mesh { asset_path: "models/sae.glb".into() });
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "fader".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);

    let t = world.get::<floptle_core::Tint>(e).copied().expect("a tint was set");
    assert_eq!(t.alpha, 0.5, "the fade is what was asked for");
    assert_eq!(
        t.color,
        [1.0, 1.0, 1.0],
        "and the model keeps its own colours — a table read as a colour arrives BLACK"
    );
}

/// **An option that is present and wrong is an error, not a skip.**
///
/// The options table used to read each field through a pattern that fell
/// through on any shape it did not expect — `if let Ok(Value::Table(ct))`.
/// So `rim = vec3(1,0,0)` set no rim and said nothing, while the same vec3
/// passed positionally is a documented spelling of a colour. Two silent
/// failures in one call shape, in the API whose whole history is silent
/// failures.
///
/// Now a colour field takes every spelling the rest of the API takes, and
/// anything else raises with the field's name in it.
#[test]
fn a_tint_option_of_the_wrong_shape_is_loud() {
    let dir = std::env::temp_dir().join(format!("floptle-tint4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A vec3 is a colour here, exactly as it is positionally.
    write_script(
        &dir,
        "vecrim",
        concat!(
            "function start(node)\n",
            "  node:setTint{ rim = vec3(1, 0.5, 0.25) }\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Mesh { asset_path: "models/sae.glb".into() });
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "vecrim".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    let t = world.get::<floptle_core::Tint>(e).copied().expect("a vec3 rim is a rim");
    assert_eq!(t.rim[0], 1.0, "a vec3 is a colour here too");
    assert!(t.rim_strength > 0.0, "and asking for one turns it on");

    // …and something that is not a colour at all names the field.
    write_script(
        &dir,
        "badrim",
        "function start(node)\n  node:setTint{ rim = \"red\" }\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Mesh { asset_path: "models/sae.glb".into() });
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "badrim".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    let said = host.drain_logs().iter().map(|l| l.msg.clone()).collect::<Vec<_>>().join("\n");
    assert!(
        said.contains("rim"),
        "a wrong rim must name the field rather than doing nothing: {said}"
    );
    assert!(
        world.get::<floptle_core::Tint>(e).is_none(),
        "and it must not have half-applied"
    );
}

/// **A clothing system, in script.** `node:materials()` says what the parts
/// are called; `node:material(name)` is one of them, read and assigned.
///
/// The ask, verbatim: *"for my clothing system I could swap the texture for
/// the arms and torso for the shirt and swap the texture for the legs for
/// the pants, and I could do that with a script."* None of it was reachable:
/// a script could set the node's material (which covers the whole model) and
/// there was no way to name one part, no way to find out what the parts were
/// called, and `mat.texture` read back nil however many times it had been
/// written.
#[test]
fn a_script_dresses_one_part_of_a_model() {
    use floptle_core::{Material, Matter, ObjectMaterials};

    let dir = std::env::temp_dir().join(format!("floptle-clothing-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "wardrobe",
        concat!(
            "function start(node)\n",
            // Discovery first — the part names are the model's, not the
            // script author's guess.
            "  for _, slot in ipairs(node:materials()) do\n",
            "    log(slot.material .. ' on ' .. slot.object .. ' textured=' .. \n",
            "        tostring(slot.textured) .. ' overridden=' .. tostring(slot.overridden))\n",
            "  end\n",
            // The shirt goes on every part wearing the Clothing material…
            "  local shirt = node:material('Clothing')\n",
            // Reading before writing: a part with no override yet reads as
            // the default material, so the ordinary first line anybody
            // writes — halve what is there — is arithmetic and not a raise.
            "  log('fresh alpha=' .. tostring(shirt.alpha))\n",
            "  shirt.alpha = shirt.alpha * 0.5\n",
            "  shirt.texture = 'art/shirt.png'\n",
            "  shirt.color = color(1, 0.9, 0.9)\n",
            // …the trousers on one named object…
            "  node:material('RightLeg#2').texture = 'art/pants.png'\n",
            // …and the whole-model Material stays what it was.
            "  node:material().roughness = 0.25\n",
            // Read-your-writes, on a string, which used to answer nil.
            "  log('wearing ' .. tostring(shirt.texture))\n",
            "end\n",
        ),
    );

    let mut world = World::default();
    let hero = world.spawn();
    world.insert(hero, Transform::IDENTITY);
    world.insert(hero, Matter::Mesh { asset_path: "models/avatar.glb".into() });
    world.insert(hero, Material::default());
    world.insert(
        hero,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "wardrobe".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );

    let mut host = ScriptHost::new();
    // What the editor lends: the parts this model was imported with.
    host.set_model_slots(std::collections::HashMap::from([(
        "models/avatar.glb".to_string(),
        vec![
            crate::ModelSlot {
                object: "Torso#2".into(),
                material: "Clothing".into(),
                textured: true,
            },
            crate::ModelSlot {
                object: "RightLeg#2".into(),
                material: "Pants".into(),
                textured: true,
            },
        ],
    )]));
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

    let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert!(
        logs.iter().any(|l| l == "Clothing on Torso#2 textured=true overridden=false"),
        "a script must be able to ASK what the parts are called: {logs:?}"
    );
    assert!(
        logs.iter().any(|l| l == "fresh alpha=1.0" || l == "fresh alpha=1"),
        "a part with no override yet reads as the material it is about to become: {logs:?}"
    );
    assert!(
        logs.iter().any(|l| l == "wearing art/shirt.png"),
        "a material's texture has to read back — a field you can only write is half a \
         field: {logs:?}"
    );

    // …and the writes landed on the component the renderer reads.
    let om = world.get::<ObjectMaterials>(hero).expect("the overrides were created");
    let shirt = om.0.get("Clothing").expect("the material-name slot");
    assert_eq!(shirt.texture.as_deref(), Some("art/shirt.png"));
    assert!((shirt.color[1] - 0.9).abs() < 1e-5, "the colour went on as a colour: {:?}", shirt.color);
    assert!((shirt.alpha - 0.5).abs() < 1e-5, "read-then-write landed: {}", shirt.alpha);
    assert_eq!(
        om.0.get("RightLeg#2").and_then(|m| m.texture.as_deref()),
        Some("art/pants.png"),
        "an object name addresses one part"
    );
    // The node's own Material is still the node's own.
    let node_mat = world.get::<Material>(hero).expect("still there");
    assert!((node_mat.roughness - 0.25).abs() < 1e-5);
    assert_eq!(node_mat.texture, None, "dressing a part must not touch the whole model");
}

/// A script points a camera at a render target and reads back what it got.
///
/// The camera group had seven entries and not one of them rendered
/// anything: `target` was settable only in the Inspector, so a minimap was
/// impossible from script even though the engine had been rendering camera
/// targets for two releases.
#[test]
fn a_script_aims_a_camera_at_a_render_target_and_sizes_it() {
    let dir = std::env::temp_dir().join(format!("floptle_rt_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "minimap",
        "\
function start(node)
  local eye = find('MapEye')
  eye:setCamera{ target = 'minimap', width = 256, height = 256, hz = 10, fovY = 1.2 }
  node:setMaterial{ texture = 'rt:minimap', unlit = true }
end
",
    );
    let (mut world, e) = world_with_script("minimap");
    // The camera is a scene node the script finds and aims, which is how a
    // game's minimap camera is authored: once, then driven.
    let eye = world.spawn();
    world.insert(eye, Transform::IDENTITY);
    world.insert(eye, floptle_core::Name("MapEye".into()));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    // The screen wears the live feed.
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let m = world.get::<floptle_core::Material>(e).expect("setMaterial ran");
    assert_eq!(m.texture.as_deref(), Some("rt:minimap"));
    // The camera the script created carries its own size and rate — not the
    // 480×270-every-frame every target used to get.
    let cam = world
        .query::<Matter>()
        .find_map(|(_, m)| match m {
            Matter::Camera { target, target_w, target_h, target_hz, fov_y, .. }
                if target == "minimap" =>
            {
                Some((*target_w, *target_h, *target_hz, *fov_y))
            }
            _ => None,
        })
        .expect("setCamera made a render-target camera");
    assert_eq!((cam.0, cam.1), (256, 256), "the size the script asked for");
    assert_eq!(cam.2, 10.0, "the rate the script asked for");
    assert!((cam.3 - 1.2).abs() < 1e-5);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every way of getting `setCamera` wrong raises at the call, naming the
/// property, the value and what is accepted.
///
/// A silently-defaulted render target is invisible: the texture resolves,
/// the picture is there, and it is simply the wrong size or rate forever.
#[test]
fn a_bad_camera_option_is_refused_where_it_was_written() {
    let dir = std::env::temp_dir().join(format!("floptle_rt_bad_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    // (what the script writes, what the message must contain)
    let cases: &[(&str, &[&str])] = &[
        ("node:setCamera{ targt = 'minimap' }", &["targt", "did you mean `target`"]),
        ("node:setCamera{ width = 0 }", &["width", "8"]),
        ("node:setCamera{ hz = '10' }", &["hz", "string"]),
        ("node:setCamera{ active = 0 }", &["active", "true or false"]),
        ("node:setCamera{ target = 42 }", &["target", "integer"]),
        // The prefix belongs to the texture ref, not to the name — this
        // would otherwise make a texture called `rt:rt:minimap`, which
        // resolves to nothing and says nothing.
        ("node:setCamera{ target = 'rt:minimap' }", &["rt:minimap", "target = \"minimap\""]),
    ];
    for (i, (src, wants)) in cases.iter().enumerate() {
        let name = format!("bad{i}");
        write_script(&dir, &name, &format!("function start(node)\n  {src}\nend\n"));
        let (mut world, _e) = world_with_script(&name);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        let errs = host.errors().to_vec();
        assert!(!errs.is_empty(), "`{src}` was accepted silently");
        let msg = errs.join(" | ");
        for want in *wants {
            assert!(msg.contains(want), "`{src}` error is missing {want:?}: {msg}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn script_reads_and_swaps_mesh_model() {
    // node.model reflects the current Mesh asset; assigning it swaps the model
    // (applied to the ECS in run + reported via take_model_changes for re-import).
    let dir = std::env::temp_dir().join("floptle_script_test_model");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "swap",
        "function update(node, dt)\n  if node.model == \"assets/models/old.glb\" then node.model = \"assets/models/new.glb\" end\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Mesh { asset_path: "assets/models/old.glb".into() });
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "swap".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    match world.get::<Matter>(e).unwrap() {
        Matter::Mesh { asset_path } => assert_eq!(asset_path, "assets/models/new.glb"),
        other => panic!("expected mesh, got {other:?}"),
    }
    let changes = host.take_model_changes();
    assert_eq!(changes.get(&e.index()).map(|s| s.as_str()), Some("assets/models/new.glb"));
}

/// **A shader knob on one part of a model, from a script**.
///
/// A model's parts can each wear a `.flsl` — skin here, a face decal there —
/// with every uniform authored in the scene, and not one of them changeable
/// at runtime: `node:setShaderParam` folded into the node's own Material,
/// which on such a model does not exist, and the part handle had no spelling
/// for a uniform or a slot at all. The reported case is a character creator
/// that wants to swap the face texture on the head.
///
/// The card's guard: two parts, a `.flsl` on both, a texture slot set on one
/// from Lua — the other's slot is unchanged and the first's resolves. Route
/// the write to the node Material instead and it fails. Read-back is asserted
/// in the same frame (the pending write) and the next (the mirror).
#[test]
fn a_part_handle_writes_its_own_shader_knobs_and_leaves_the_other_parts_alone() {
    let dir = std::env::temp_dir().join("floptle_script_test_part_shader");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "creator",
        concat!(
            "frame = 0\n",
            "function update(node, dt)\n",
            "  frame = frame + 1\n",
            "  local head = node:material(\"Head#2\")\n",
            "  if frame == 1 then\n",
            "    head:setShaderTexture(\"face\", \"faces/02.png\")\n",
            "    head:setShaderParam(\"glow\", 2, 0.5)\n",
            "    -- the same frame: the pending write answers\n",
            "    assert(head:shaderTexture(\"face\") == \"faces/02.png\", 'pending texture')\n",
            "    local x, y, z, w = head:shaderParam(\"glow\")\n",
            "    assert(x == 2 and y == 0.5 and z == 0 and w == 0, 'pending param')\n",
            "    assert(node:material(\"Torso#1\"):shaderTexture(\"face\") == \"faces/01.png\", 'torso')\n",
            "  else\n",
            "    -- the next frame: the mirror answers\n",
            "    assert(head:shaderTexture(\"face\") == \"faces/02.png\", 'mirrored texture')\n",
            "    local x, y = head:shaderParam(\"glow\")\n",
            "    assert(x == 2 and y == 0.5, 'mirrored param')\n",
            "    assert(head:shaderParam(\"nothing\") == nil, 'an unset knob is nil')\n",
            "    print('read back')\n",
            "  end\n",
            "end\n",
        ),
    );
    let part = |face: &str| Material {
        shader: Some("shaders/skin.flsl".into()),
        shader_textures: [("face".to_string(), face.to_string())].into_iter().collect(),
        ..Default::default()
    };
    let mut world = World::default();
    let hero = world.spawn();
    world.insert(hero, Transform::IDENTITY);
    world.insert(hero, floptle_core::Name("Hero".into()));
    world.insert(
        hero,
        floptle_core::ObjectMaterials(
            [("Head#2".to_string(), part("faces/01.png")), ("Torso#1".to_string(), part("faces/01.png"))]
                .into_iter()
                .collect(),
        ),
    );
    world.insert(
        hero,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "creator".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let om = world.get::<floptle_core::ObjectMaterials>(hero).unwrap();
    assert_eq!(om.0["Head#2"].shader_textures.get("face").map(String::as_str), Some("faces/02.png"));
    assert_eq!(om.0["Head#2"].shader_params.get("glow"), Some(&[2.0, 0.5, 0.0, 0.0]));
    assert_eq!(
        om.0["Torso#1"].shader_textures.get("face").map(String::as_str),
        Some("faces/01.png"),
        "the other part's slot moved"
    );
    assert!(om.0["Torso#1"].shader_params.is_empty());
    assert!(world.get::<Material>(hero).is_none(), "a node Material was invented");

    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert!(said.iter().any(|m| m == "read back"), "{said:?}");
}

/// **The node-level call on a model with part overrides and no node
/// Material** fans out to every part that wears a shader — and with none
/// to write to, says so once rather than nothing. A part
/// write with no override never creates one: an override is a whole
/// material, and a uniform must not be able to blank a part.
#[test]
fn a_node_level_shader_write_fans_out_to_the_parts_that_wear_a_shader_or_says_so_once() {
    let dir = std::env::temp_dir().join("floptle_script_test_part_shader_fanout");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "tinter",
        concat!(
            "function update(node, dt)\n",
            "  node:setShaderParam(\"tint\", 1)\n",
            "  node:setShaderTexture(\"ramp\", \"ramps/hot.png\")\n",
            "  find(\"Bare\"):setShaderParam(\"tint\", 1)\n",
            "  node:material(\"Nope\"):setShaderParam(\"glow\", 1)\n",
            "end\n",
        ),
    );
    let shaded = Material { shader: Some("shaders/x.flsl".into()), ..Default::default() };
    let mut world = World::default();
    let hero = world.spawn();
    world.insert(hero, Transform::IDENTITY);
    world.insert(hero, floptle_core::Name("Hero".into()));
    world.insert(
        hero,
        floptle_core::ObjectMaterials(
            [
                ("Head#2".to_string(), shaded.clone()),
                ("Torso#1".to_string(), shaded.clone()),
                ("Belt#3".to_string(), Material::default()),
            ]
            .into_iter()
            .collect(),
        ),
    );
    world.insert(
        hero,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "tinter".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    // A model whose overrides wear no shader at all.
    let bare = world.spawn();
    world.insert(bare, Transform::IDENTITY);
    world.insert(bare, floptle_core::Name("Bare".into()));
    world.insert(
        bare,
        floptle_core::ObjectMaterials([("Only#1".to_string(), Material::default())].into_iter().collect()),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let om = world.get::<floptle_core::ObjectMaterials>(hero).unwrap();
    for p in ["Head#2", "Torso#1"] {
        assert_eq!(om.0[p].shader_params.get("tint"), Some(&[1.0, 0.0, 0.0, 0.0]), "{p}");
        assert_eq!(om.0[p].shader_textures.get("ramp").map(String::as_str), Some("ramps/hot.png"), "{p}");
    }
    assert!(om.0["Belt#3"].shader_params.is_empty(), "a part with no shader was written to");
    assert!(!om.0.contains_key("Nope"), "a part write invented an override");
    assert!(world.get::<Material>(hero).is_none());

    // Two frames, two kinds of nowhere, each said exactly once.
    let warned: Vec<String> = host
        .drain_logs()
        .into_iter()
        .filter(|l| matches!(l.level, LogLevel::Warn))
        .map(|l| l.msg)
        .collect();
    assert_eq!(warned.len(), 2, "{warned:#?}");
    assert!(warned.iter().any(|m| m.contains("\"Bare\"") && m.contains("no part wears a shader")), "{warned:#?}");
    assert!(warned.iter().any(|m| m.contains("\"Nope\"") && m.contains("no material override")), "{warned:#?}");
}

/// the sky's uniforms are a third place, and until this they
/// were the only shader in the engine a script could not talk to. A
/// procedural sky that can only be a function of `time` runs its story on a
/// clock — the reported case was a cutscene sky whose city was revealed in
/// the middle of whatever sentence the reader happened to be on.
#[test]
fn set_shader_param_reaches_the_sky() {
    let dir = std::env::temp_dir().join("floptle_script_test_sky_param");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "story",
        concat!(
            "function update(node, dt)\n",
            "  local sky = find(\"Skybox\")\n",
            "  sky:setShaderParam(\"burn\", 0.75)\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let sky = world.spawn();
    world.insert(sky, Transform::IDENTITY);
    world.insert(sky, floptle_core::Name("Skybox".into()));
    world.insert(
        sky,
        Matter::Skybox {
            color: [0.0; 3],
            size: 1000.0,
            texture: None,
            tint: [1.0; 3],
            shader: Some("shaders/ashfall.flsl".into()),
            shader_params: Default::default(),
        },
    );
    // A sky node that also carries a material: the write must still go where
    // the sky pipeline reads, not into the material nobody draws.
    world.insert(sky, Material { shader: Some("shaders/x.flsl".into()), ..Default::default() });
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "story".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let Some(Matter::Skybox { shader_params, .. }) = world.get::<Matter>(sky) else {
        panic!("the sky lost its matter")
    };
    assert_eq!(shader_params.get("burn"), Some(&[0.75, 0.0, 0.0, 0.0]));
    assert!(
        world.get::<Material>(sky).unwrap().shader_params.is_empty(),
        "the write went to the material instead of the sky"
    );
}

/// Sorting layers and 2D lighting without script access would rule out the
/// ordinary 2D moves — a
/// character stepping behind a counter, a torch that stops lighting the
/// background. A misspelled enum has to name the accepted set rather than
/// quietly meaning `auto`.
#[test]
fn a_script_drives_sorting_and_2d_lighting() {
    let dir = std::env::temp_dir().join("floptle_script_test_sort2d");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "stack",
        concat!(
            "function start(node)\n",
            "  node:setSorting{ layer = \"Characters\", order = 3 }\n",
            "  node:setLighting2D{ mode = \"2d\", blocks = \"on\" }\n",
            "  local torch = find(\"Torch\")\n",
            "  torch:setLighting2D{ mode = \"2d\", layers = { \"Characters\" } }\n",
            "  ok, err = pcall(function() torch:setLighting2D{ mode = \"flat-ish\" } end)\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let hero = world.spawn();
    world.insert(hero, Transform::IDENTITY);
    world.insert(
        hero,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "stack".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let torch = world.spawn();
    world.insert(torch, Transform::IDENTITY);
    world.insert(torch, floptle_core::Name("Torch".into()));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

    let s = world.get::<floptle_core::Sorting>(hero).expect("sorting was not set");
    assert_eq!((s.layer.as_str(), s.order), ("Characters", 3));
    let lit = world.get::<floptle_core::Lighting2D>(hero).expect("no Lighting2D");
    assert_eq!(lit.mode, floptle_core::Lit2D::Yes);
    assert!(lit.layers.is_empty(), "a receiver names no layers");
    assert_eq!(
        world.get::<floptle_core::Shadow2D>(hero).map(|c| c.0),
        Some(floptle_core::Cast2D::Yes)
    );
    let torch_lit = world.get::<floptle_core::Lighting2D>(torch).expect("no Lighting2D");
    assert_eq!(torch_lit.layers, vec!["Characters".to_string()]);
    // …and the bad spelling raised rather than defaulting. `pcall` caught it,
    // so the run itself is still clean — which is the point: the script
    // author hears about the typo, the engine does not guess.
    assert_eq!(
        torch_lit.mode,
        floptle_core::Lit2D::Yes,
        "the refused write must not have changed anything"
    );
}

/// The other half: the post chain is typed knobs rather than
/// a shader's uniforms, so it comes through the component route. A cutscene
/// pushing a vignette is the reported want.
#[test]
fn script_drives_the_post_chain() {
    let dir = std::env::temp_dir().join("floptle_script_test_post");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "cut",
        concat!(
            "function update(node, dt)\n",
            "  local pp = find(\"Post\"):getcomponent(\"PostProcess\")\n",
            // Bloom is off in this scene, so this branch must not be taken.
            // If the field arrived as the number 0 instead of `false` it
            // would be — 0 is truthy in Lua — and the assertion below is
            // what catches that.
            "  if pp.bloom then pp.bloomIntensity = 2.5 end\n",
            "  pp.vignette = 1\n",
            "  pp.vignetteStrength = 0.8\n",
            "  pp.posterizeBands = -4\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let post = world.spawn();
    world.insert(post, Transform::IDENTITY);
    world.insert(post, floptle_core::Name("Post".into()));
    world.insert(post, Matter::default_post_process());
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "cut".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let Some(Matter::PostProcess {
        bloom_intensity, vignette, vignette_strength, posterize_bands, ..
    }) = world.get::<Matter>(post)
    else {
        panic!("the post node lost its matter")
    };
    assert_eq!(
        *bloom_intensity, 0.7,
        "bloom is off, so `if pp.bloom` must be false — 0 would have been truthy"
    );
    assert!(*vignette);
    assert_eq!(*vignette_strength, 0.8);
    assert_eq!(*posterize_bands, 0, "a negative band count must floor at off, not wrap");
}

/// A settings screen switches off the lens and film effects players ask to
/// lose, and resets the grade. Each value read here differs from the default,
/// so a field that is missing from the mirror cannot pass as a default.
#[test]
fn a_settings_screen_reaches_the_lens_and_the_grade() {
    let dir = std::env::temp_dir().join("floptle_script_test_post_settings");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "settings",
        concat!(
            "function update(node, dt)\n",
            "  local pp = find(\"Post\"):getComponent(\"PostProcess\")\n",
            "  assert(math.abs(pp.aberration - 0.4) < 1e-6, 'aberration ' .. tostring(pp.aberration))\n",
            "  assert(math.abs(pp.distortion + 0.09) < 1e-6, 'distortion ' .. tostring(pp.distortion))\n",
            "  assert(math.abs(pp.grain - 0.3) < 1e-6, 'grain ' .. tostring(pp.grain))\n",
            "  assert(math.abs(pp.gradeGamma - 1.4) < 1e-6, 'gradeGamma ' .. tostring(pp.gradeGamma))\n",
            "  assert(math.abs(pp.saturation - 0.5) < 1e-6, 'saturation ' .. tostring(pp.saturation))\n",
            // Feature detection: a field the component lacks is nil.
            "  assert(pp.noSuchKnob == nil)\n",
            "  pp.aberration = 0\n",
            "  pp.distortion = -3\n",
            "  pp.grain = 0\n",
            "  pp.gradeGamma, pp.saturation, pp.contrast = 1, 1, 9\n",
            "  pp.temperature, pp.exposure = 0, 0\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let post = world.spawn();
    world.insert(post, Transform::IDENTITY);
    world.insert(post, floptle_core::Name("Post".into()));
    let mut pp = Matter::default_post_process();
    if let Matter::PostProcess {
        aberration,
        distortion,
        grain,
        grade_gamma,
        saturation,
        temperature,
        exposure,
        ..
    } = &mut pp
    {
        *aberration = 0.4;
        *distortion = -0.09;
        *grain = 0.3;
        *grade_gamma = 1.4;
        *saturation = 0.5;
        *temperature = 0.6;
        *exposure = 1.5;
    }
    world.insert(post, pp);
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "settings".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let Some(Matter::PostProcess {
        aberration,
        distortion,
        grain,
        grade_gamma,
        saturation,
        contrast,
        temperature,
        exposure,
        ..
    }) = world.get::<Matter>(post)
    else {
        panic!("the post node lost its matter")
    };
    assert_eq!(*aberration, 0.0);
    assert_eq!(*distortion, -0.5, "distortion is signed, clamped to the Inspector's -0.5");
    assert_eq!(*grain, 0.0);
    assert_eq!(*grade_gamma, 1.0);
    assert_eq!(*saturation, 1.0);
    assert_eq!(*contrast, 3.0, "clamped to the Inspector's range");
    assert_eq!(*temperature, 0.0);
    assert_eq!(*exposure, 0.0);
}

#[test]
fn a_script_paints_with_colors_and_reads_booleans_as_booleans() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_color");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "paint",
        concat!(
            "function update(node, dt)\n",
            "  local el = node:getcomponent(\"UiElement\")\n",
            // One line instead of four channel pokes.
            "  el.fill = color(1, 0.5, 0.25)\n",
            "  el.textColor = color.hex(\"#3366ccff\")\n",
            // …and a boolean that behaves like one. `visible` starts
            // false; if it read back as the number 0 this branch would be
            // taken, because 0 is truthy in Lua. That is the bug.
            "  if el.visible then node.x = 99 else node.x = 1 end\n",
            "  el.visible = true\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        floptle_ui::ElementSpec {
            visible: false,
            shape: Some(floptle_ui::ShapeSpec::default()),
            text: Some(floptle_ui::TextSpec { text: "hi".into(), ..Default::default() }),
            ..Default::default()
        },
    );
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "paint".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.x,
        1.0,
        "`if el.visible` must be false when it is false — 0 is truthy in Lua"
    );
    let spec = world.get::<floptle_ui::ElementSpec>(e).unwrap();
    let fill = spec.shape.as_ref().unwrap().fill;
    assert!((fill[0] - 1.0).abs() < 1e-6 && (fill[1] - 0.5).abs() < 1e-6);
    assert_eq!(fill[3], 1.0, "a three-argument color is opaque, not invisible");
    let tc = spec.text.as_ref().unwrap().color;
    assert!((tc[0] - 0x33 as f32 / 255.0).abs() < 1e-4, "hex parsed: {tc:?}");
    assert!(spec.visible);
}

/// The other half: a non-string raises instead of being dropped. A write
/// that silently does nothing is the disease; the wrong portrait was the
/// symptom.
#[test]
fn a_non_string_texture_raises() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_texture_bad");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "bad", "function update(node, dt)\n  node.texture = 42\nend\n");
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, floptle_ui::ElementSpec::default());
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "bad".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(
        host.errors().iter().any(|e| e.contains("texture")),
        "a bad texture write must say so: {:?}",
        host.errors()
    );
}

#[test]
fn script_applies_material_preset() {
    // node.material = "<name>" resolves against the lent presets and inserts a Material.
    let dir = std::env::temp_dir().join("floptle_script_test_material");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "paint", "function update(node, dt)\n  node.material = \"Gold\"\nend\n");
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Mesh { asset_path: "m.glb".into() });
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "paint".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    let mut mats = HashMap::new();
    mats.insert("Gold".to_string(), Material::tinted([1.0, 0.84, 0.0]));
    host.set_materials(mats);
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let mat = world.get::<Material>(e).expect("material applied");
    assert_eq!(mat.color, [1.0, 0.84, 0.0]);
}

#[test]
fn script_toggles_visibility() {
    // node.visible reads true by default; assigning false attaches Visible(false).
    let dir = std::env::temp_dir().join("floptle_script_test_visible");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "hide",
        "function update(node, dt)\n  if node.visible then node.visible = false end\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Mesh { asset_path: "m.glb".into() });
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "hide".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Visible>(e).copied(), Some(Visible(false)));
}

/// The colour spellings the docs promise all reach the component. `{r,g,b}` was
/// documented in `floptle.lua` and named in the converter's own error message, and
/// was the one shape it refused.
#[test]
fn set_material_accepts_every_documented_colour_shape() {
    let dir = std::env::temp_dir().join("floptle_script_test_colour_shapes");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "paint",
        "function update(node, dt)\n\
           node:setMaterial{ color = { r = 1, g = 0.5, b = 0.25 } }\n\
           find(\"B\"):setMaterial{ color = { x = 1, y = 0.5, z = 0.25 } }\n\
           find(\"C\"):setMaterial{ color = { 1, 0.5, 0.25 } }\n\
           find(\"D\"):setMaterial{ color = vec3(1, 0.5, 0.25) }\n\
         end\n",
    );
    let mut world = World::default();
    let mut nodes = Vec::new();
    for name in ["A", "B", "C", "D"] {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_core::Name(name.into()));
        world.insert(e, Matter::Empty);
        nodes.push(e);
    }
    world.insert(
        nodes[0],
        Scripts(vec![floptle_core::ScriptInst {
            kind: "paint".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    for (e, name) in nodes.iter().zip(["{r,g,b}", "{x,y,z}", "array", "vec3"]) {
        let m = world.get::<floptle_core::Material>(*e).expect(name);
        assert!(
            (m.color[0] - 1.0).abs() < 1e-5
                && (m.color[1] - 0.5).abs() < 1e-5
                && (m.color[2] - 0.25).abs() < 1e-5,
            "{name} did not reach the material: {:?}",
            m.color
        );
    }
}
