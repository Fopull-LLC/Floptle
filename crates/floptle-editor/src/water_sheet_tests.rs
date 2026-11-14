//! A sheet of simulated water, built once and rewritten in place: the whole
//! path a game takes, from the engine's solver through `water.surface` and
//! `mesh.update` to the driver.

use floptle_core::math::Vec3;
use floptle_core::Transform;

/// A flat floor at y = 0 with a 17 × 17 m pool over it filled to 1 m, and a
/// script that keeps one sheet in step with it, counting what it does.
fn pool_project(name: &str) -> (crate::Editor, floptle_core::Entity, floptle_core::Entity, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("floptle-water-sheet-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
    floptle_vfs::write(
        dir.join("scripts/sheet.lua"),
        r#"local id = water.addVolume{ shape = "sim", origin = vec3(-8, 0, -8), cell = 1, columns = 17, rows = 17, level = 1 }
builds, updates, skipped, maxFoam, centreDepth, verts = 0, 0, 0, 0, 0, 0
function update(node, dt)
  if not water.surfaceChanged(id) then
    skipped = skipped + 1
    return
  end
  local s = water.surface(id, { fixed = true, colors = true, buffers = true })
  verts = buffer.len(s.positions) // 12
  for k = 0, verts - 1 do
    maxFoam = math.max(maxFoam, buffer.readf32(s.colors, k * 16 + 12))
  end
  centreDepth = buffer.readf32(s.colors, (8 * 17 + 8) * 16) * 16
  if sheet then
    mesh.update(sheet, s)
    updates = updates + 1
  else
    sheet = mesh.build(s)
    node.model = sheet
    builds = builds + 1
  end
  meshes = perf.counts().scriptMeshes
end
"#,
    )
    .unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    let mut field = floptle_field::ChunkField::new(0.5);
    field.fill_slab(Vec3::new(-12.0, -6.0, -12.0), Vec3::new(12.0, 0.0, 12.0), 0.0, [0.5, 0.5, 0.5]);
    let terrain = ed.world.spawn();
    ed.world.insert(terrain, Transform::IDENTITY);
    ed.terrains.insert(terrain, crate::terrain_edit::EditorTerrain::new(field));
    let host = ed.world.spawn();
    ed.world.insert(host, Transform::IDENTITY);
    ed.world.insert(host, floptle_core::Scripts(vec![floptle_core::ScriptInst::new("sheet")]));
    (ed, terrain, host, dir)
}

/// **A sheet follows water the engine moves, rewritten in place.** Built
/// once, it is updated while the water moves and left alone once it settles;
/// its colours carry the sim's own depth, and foam where the water pours
/// into a pit dug beside it. However long it runs there is one script mesh.
#[test]
fn a_sheet_follows_simulated_water_without_being_rebuilt() {
    use floptle_field::{Brush, BrushProfile};
    let (mut ed, terrain, host, dir) = pool_project("follow");
    ed.toggle_play();
    for _ in 0..120 {
        ed.play_step(1.0 / 60.0, true);
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    let env = ed.script_host.instance_env(host.index(), "sheet").unwrap();
    let num = |key: &str| env.get::<f64>(key).unwrap();
    assert_eq!(num("builds"), 1.0, "the sheet was built once");
    assert_eq!(num("verts"), 289.0, "every point of the 17 × 17 grid is a vertex");
    assert!((num("centreDepth") - 0.95).abs() < 0.1, "a vertex's red is the water's depth: {} m", num("centreDepth"));
    assert!(num("maxFoam") < 0.05, "still water foamed: {}", num("maxFoam"));
    // The pool is at rest: nothing has changed for most of those frames, so
    // nothing was written.
    assert!(num("skipped") > 60.0, "a still pool was rewritten every frame ({} frames skipped)", num("skipped"));
    let quiet = num("updates");

    // A pit 2 m deep at the pool's edge: the water pours in.
    let touched = {
        let t = ed.terrains.get_mut(&terrain).unwrap();
        t.field.sculpt(Brush::Lower, Vec3::new(6.0, 0.0, 0.0), 2.0, 1.0, BrushProfile { hardness: 1.0, ..Default::default() })
    };
    ed.mirror_terrain_chunks_to_sim(terrain, &touched);
    for _ in 0..4 {
        ed.pump_collision_meshes();
    }
    for _ in 0..240 {
        ed.play_step(1.0 / 60.0, true);
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert!(num("updates") > quiet + 20.0, "moving water did not update the sheet ({} → {})", quiet, num("updates"));
    assert!(num("maxFoam") > 0.5, "water pouring into a pit carried no foam: {}", num("maxFoam"));
    assert_eq!(num("builds"), 1.0, "the sheet was rebuilt");
    assert_eq!(num("meshes"), 1.0, "updates left script meshes behind");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The driver writes an update into the mesh that is there.** With a GPU:
/// the model keeps its mesh and its buffers, the new triangle count is what
/// draws from the very pump the update was made before, and nothing is
/// registered or released.
#[test]
fn an_update_is_written_into_the_meshes_own_buffers() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-mesh-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("maker.lua"),
        "function start(node)\n\
         \x20 quad = mesh.build{ positions = {0,0,0, 1,0,0, 0,1,0, 1,1,0}, indices = {1,2,3}, colors = {1,0,0,1, 1,0,0,1, 1,0,0,1, 1,0,0,1},\n\
         \x20                    dynamic = true, indexCapacity = 6 }\n\
         \x20 node.model = quad\n\
         end\n\
         local f = 0\n\
         function update(node, dt)\n\
         \x20 f = f + 1\n\
         \x20 if f == 2 then mesh.update(quad, { positions = {0,0,0, 4,0,0, 0,4,0, 4,4,0}, indices = {1,2,3, 2,4,3}, colors = {0,1,0,1, 0,1,0,1, 0,1,0,1, 0,1,0,1} }) end\n\
         \x20 if f == 3 then mesh.update(quad, { indices = {} }) end\n\
         end\n",
    )
    .unwrap();
    let mut world = floptle_core::World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, floptle_core::Scripts(vec![floptle_core::ScriptInst::new("maker")]));
    let mut frame = |ed: &mut crate::Editor, i: usize| {
        ed.script_host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        ed.pump_runtime_textures();
    };
    frame(&mut ed, 0);
    let name = ed.mesh_registry.keys().find(|k| k.starts_with("mesh:")).cloned().expect("the quad is a model");
    let id = ed.mesh_registry[&name].parts[0];
    let raster = ed.raster.as_ref().unwrap();
    let buffers = raster.mesh_buffers(id).map(|(v, i)| (v.clone(), i.clone())).unwrap();
    assert_eq!(buffers.1.size(), 6 * 4, "the index buffer has the room it was built with");
    assert_eq!(raster.mesh_index_count(id), 3);
    assert_eq!(raster.paint_get(raster.mesh_paint_base(id), 0), [255, 0, 0, 255]);
    let size = ed.mesh_registry[&name].size;

    frame(&mut ed, 1);
    let raster = ed.raster.as_ref().unwrap();
    assert_eq!(ed.mesh_registry[&name].parts, vec![id], "the update made another mesh");
    assert_eq!(raster.mesh_rewrites(), 1);
    assert_eq!(raster.mesh_index_count(id), 6, "the new triangles do not draw in the frame they were given");
    assert_eq!(raster.paint_get(raster.mesh_paint_base(id), 3), [0, 255, 0, 255], "the new colours were not written");
    let now = raster.mesh_buffers(id).unwrap();
    assert!(*now.0 == buffers.0 && *now.1 == buffers.1, "the update made new GPU buffers");
    assert!(ed.mesh_registry[&name].size > size * 3.0, "the model's size did not follow its vertices");

    frame(&mut ed, 2);
    let raster = ed.raster.as_ref().unwrap();
    assert_eq!(raster.mesh_index_count(id), 0, "a sheet with no triangles still draws");
    assert_eq!(ed.mesh_registry.keys().filter(|k| k.starts_with("mesh:")).count(), 1);
    ed.release_runtime_textures();
    let _ = std::fs::remove_dir_all(&dir);
}

/// **What a script writes is what the next picture shows, and no picture is
/// empty.** A game frame at a time, as a build runs it: a quad a script built
/// is moved and recoloured by `mesh.update`, then cut to one triangle, then
/// to none, then filled again. Each change is in the picture of the frame the
/// script made it in, and every frame from the one after the build shows the
/// mesh as it was or as it is now, never nothing. Set
/// `FLOPTLE_KEEP_SHOTS=<dir>` to keep the frames as PNGs.
#[test]
fn an_update_shows_in_the_picture_of_the_frame_it_was_made_in() {
    use floptle_core::{Material, Matter, Name, ScriptInst, Scripts};
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-mesh-update-shot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
    floptle_vfs::write(
        dir.join("scripts/quad.lua"),
        "local red, green = {1,0,0,1, 1,0,0,1, 1,0,0,1, 1,0,0,1}, {0,1,0,1, 0,1,0,1, 0,1,0,1, 0,1,0,1}\n\
         local left, right = {-1,-1,0, -0.1,-1,0, -1,1,0, -0.1,1,0}, {0.1,-1,0, 1,-1,0, 0.1,1,0, 1,1,0}\n\
         frame = 0\n\
         function update(node, dt)\n\
         \x20 frame = frame + 1\n\
         \x20 if frame == 1 then\n\
         \x20   quad = mesh.build{ positions = left, indices = {1,2,3, 2,4,3}, colors = red, dynamic = true }\n\
         \x20   node.model = quad\n\
         \x20 elseif frame == 4 then mesh.update(quad, { positions = right, colors = green })\n\
         \x20 elseif frame == 6 then mesh.update(quad, { indices = {1,2,3} })\n\
         \x20 elseif frame == 8 then mesh.update(quad, { indices = {} })\n\
         \x20 elseif frame == 9 then mesh.update(quad, { indices = {1,2,3, 2,4,3} })\n\
         \x20 end\n\
         end\n",
    )
    .unwrap();
    ed.project_root = dir.clone();
    let cam = ed.world.spawn();
    ed.world.insert(cam, Transform::from_translation(floptle_core::math::DVec3::new(0.0, 0.0, 1.5)));
    ed.world.insert(cam, Name("Camera".into()));
    ed.world.insert(
        cam,
        Matter::Camera {
            fov_y: 1.0,
            active: true,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: 0,
            target_h: 0,
            target_hz: 0.0,
            ortho: false,
            ortho_height: 10.0,
            stack: Vec::new(),
            draws: floptle_core::CameraDraws::default(),
            post_after: false,
            near: 0.05,
            far: 100.0,
        },
    );
    let host = ed.world.spawn();
    ed.world.insert(host, Transform::IDENTITY);
    ed.world.insert(host, Name("Quad".into()));
    ed.world.insert(host, Matter::Empty);
    ed.world.insert(host, Material { unlit: true, ..Default::default() });
    ed.world.insert(host, Scripts(vec![ScriptInst::new("quad")]));
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");

    const W: u32 = 96;
    const H: u32 = 64;
    let (color, depth) = {
        let gpu = ed.gpu.as_ref().unwrap();
        crate::viewports::offscreen_textures(gpu, W, H, "mesh-update-shot", wgpu::TextureUsages::COPY_SRC)
    };
    let keep = std::env::var("FLOPTLE_KEEP_SHOTS").ok();
    let shot = |ed: &mut crate::Editor, n: usize| -> Vec<u8> {
        let _ = ed.player_frame(false);
        let (cv, dv) = (color.create_view(&Default::default()), depth.create_view(&Default::default()));
        ed.render_game_into(cv, dv, Some(depth.clone()), [0.0, 0.0], [W as f32, H as f32], 0.0, true, false);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let px = crate::offscreen::read_back_frame(ed.gpu.as_ref().unwrap(), &color).unwrap();
        if let Some(dir) = &keep {
            std::fs::create_dir_all(dir).ok();
            image::RgbaImage::from_raw(W, H, px.clone()).unwrap().save(format!("{dir}/mesh_update_frame_{n}.png")).unwrap();
        }
        px
    };
    // A pixel a quarter of the way in from the left or right, a quarter up or
    // down from the middle.
    let at = |px: &[u8], right: bool, up: bool| -> [u8; 4] {
        let x = if right { W * 3 / 4 } else { W / 4 };
        let y = if up { H / 4 } else { H * 3 / 4 };
        let i = ((y * W + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    };
    let red = |p: [u8; 4]| p[0] > 150 && p[1] < 80 && p[2] < 80;
    let green = |p: [u8; 4]| p[1] > 150 && p[0] < 80 && p[2] < 80;
    let lit = |p: [u8; 4]| red(p) || green(p);

    let mut frames = Vec::new();
    for n in 1..=10 {
        frames.push(shot(&mut ed, n));
    }
    let f = |n: usize| &frames[n - 1];
    // The build draws from the frame after it.
    for n in 2..=3 {
        assert!(red(at(f(n), false, false)) && red(at(f(n), false, true)), "frame {n}: the built quad is not drawn red on the left");
        assert!(!lit(at(f(n), true, false)), "frame {n}: something drew on the right");
    }
    // Moved and recoloured, in the very frame the script did it.
    for n in 4..=5 {
        assert!(!lit(at(f(n), false, false)), "frame {n}: the quad's old place still draws: {:?}", at(f(n), false, false));
        assert!(green(at(f(n), true, false)) && green(at(f(n), true, true)), "frame {n}: the moved quad is not green on the right: {:?}", at(f(n), true, false));
    }
    // One triangle: the lower-left half of the quad.
    assert!(green(at(f(6), true, false)), "frame 6: the triangle kept is gone");
    assert!(!lit(at(f(6), true, true)), "frame 6: the triangle taken away still draws: {:?}", at(f(6), true, true));
    // None, then both again.
    assert!(!lit(at(f(8), true, false)) && !lit(at(f(8), true, true)), "frame 8: a quad with no triangles drew");
    assert!(green(at(f(9), true, false)) && green(at(f(9), true, true)), "frame 9: the triangles given back do not draw");
    // Every frame after the build but the one the script emptied shows it.
    for n in (2..=10).filter(|&n| n != 8) {
        assert!(f(n).as_chunks::<4>().0.iter().any(|p| lit(*p)), "frame {n} is blank");
    }
    assert_eq!(ed.mesh_registry.keys().filter(|k| k.starts_with("mesh:")).count(), 1, "an update registered another mesh");
    let _ = std::fs::remove_dir_all(&dir);
}
