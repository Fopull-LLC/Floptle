//! **A Tiled or LDtk level, placed in the scene.**
//!
//! A level becomes one node named after it, holding one tile layer per layer
//! of the original and one empty node per object or entity, tagged with its
//! class so a script can `findTagged` the spawn points. Its sheets become one
//! tileset with a page per image, so every layer paints from the same palette
//! and collision is worked out from the art, the same as a spritesheet dropped
//! on the ◫ Tiles tab.
//!
//! One tile is one world unit, so the level's tile size in pixels is its
//! pixels per unit, and sprites dropped beside it match it.

use std::path::{Path, PathBuf};

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::Matter;

use crate::level_import::{Level, Sheet};
use crate::Editor;

pub(crate) fn is_level_file(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".tmx") || l.ends_with(".ldtk")
}

/// A sheet's tiles redrawn edge to edge, with its margin and spacing taken out.
fn clean_grid(px: &[u8], w: u32, h: u32, s: &Sheet) -> Vec<u8> {
    let (gw, gh) = (s.cols * s.tile_w, s.rows * s.tile_h);
    let mut out = vec![0u8; (gw * gh * 4) as usize];
    for r in 0..s.rows {
        for c in 0..s.cols {
            let (sx0, sy0) = (s.margin + c * (s.tile_w + s.spacing), s.margin + r * (s.tile_h + s.spacing));
            for y in 0..s.tile_h {
                for x in 0..s.tile_w {
                    let (sx, sy) = (sx0 + x, sy0 + y);
                    if sx >= w || sy >= h {
                        continue;
                    }
                    let so = ((sy * w + sx) * 4) as usize;
                    let to = (((r * s.tile_h + y) * gw + c * s.tile_w + x) * 4) as usize;
                    out[to..to + 4].copy_from_slice(&px[so..so + 4]);
                }
            }
        }
    }
    out
}

impl Editor {
    /// Read a `.tmx` or `.ldtk` file and place its level(s) in the scene, at
    /// `at` when given.
    pub(crate) fn import_level_file(&mut self, path: &str, at: Option<DVec3>) {
        let file = PathBuf::from(path);
        let stem = file.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let read = floptle_vfs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|text| {
            if path.to_ascii_lowercase().ends_with(".ldtk") {
                crate::level_import::read_ldtk(&text, &file)
            } else {
                crate::level_import::read_tmx(&text, &file).map(|l| vec![l])
            }
        });
        let levels = match read {
            Ok(l) => l,
            Err(why) => {
                self.console.push(floptle_script::LogLevel::Error, format!("importing {stem}: {why}"), None);
                self.toast = Some((format!("✖  {stem} — see the Console"), 8.0));
                return;
            }
        };
        self.record();
        let mut roots = Vec::new();
        for lv in &levels {
            if let Some(root) = self.place_level(lv, &stem, at.filter(|_| levels.len() == 1)) {
                roots.push(root);
            }
        }
        self.save_tilesets();
        self.asset_tree = crate::assets::build_assets(&self.project_root);
        self.scene_dirty = true;
        if let Some(&r) = roots.first() {
            self.select_single(r);
        }
        self.toast = Some((format!("▦  {} level(s) from {stem}", roots.len()), 4.0));
    }

    /// The project path of a sheet's image, writing a copy into the project
    /// when it lives outside it or has gaps between its tiles.
    fn level_sheet_texture(&mut self, s: &Sheet, level: &str, stem: &str) -> Option<String> {
        let Some((px, w, h)) = floptle_image::io::load_image(&s.image) else {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("importing {stem}: could not read {}, so its tiles are left out", s.image.display()),
                None,
            );
            return None;
        };
        let inside = s.image.canonicalize().ok().zip(self.project_root.canonicalize().ok()).is_some_and(|(i, r)| i.starts_with(r));
        if inside && s.margin == 0 && s.spacing == 0 {
            return Some(crate::assets::asset_rel_path(&s.image.to_string_lossy(), &self.project_root));
        }
        let name = s.image.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "tiles".into());
        let out = if inside {
            s.image.with_file_name(format!("{name}.grid.png"))
        } else {
            self.project_root.join("textures").join(format!("{level}_{name}.png"))
        };
        let tex = floptle_render::TextureData {
            pixels: clean_grid(&px, w, h, s),
            width: s.cols * s.tile_w,
            height: s.rows * s.tile_h,
        };
        let _ = floptle_vfs::create_dir_all(out.parent().unwrap_or(Path::new(".")));
        match floptle_assets::encode_png(&tex).map(|b| floptle_vfs::write(&out, b)) {
            Some(Ok(())) => Some(crate::assets::asset_rel_path(&out.to_string_lossy(), &self.project_root)),
            _ => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("importing {stem}: could not write {}", out.display()),
                    None,
                );
                None
            }
        }
    }

    fn place_level(&mut self, lv: &Level, stem: &str, at: Option<DVec3>) -> Option<floptle_core::Entity> {
        for w in &lv.warnings {
            self.console.push(floptle_script::LogLevel::Warn, format!("importing {stem}: {w}"), None);
        }
        if lv.tile_w != lv.tile_h {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "importing {stem}: its tiles are {}×{} pixels; tile layers are square, so they are drawn {0}×{0}",
                    lv.tile_w, lv.tile_h
                ),
                None,
            );
        }
        let ppu = lv.tile_w.max(1) as f64;
        let level = if lv.name.trim().is_empty() { "level".to_string() } else { lv.name.clone() };

        // The sheets, as the pages of one tileset. A sheet that cannot be read
        // keeps its page number, so the pages after it still line up.
        let mut pages: Vec<Option<(String, u32, u32)>> = Vec::new();
        for s in &lv.sheets {
            pages.push(self.level_sheet_texture(s, &level, stem).map(|t| (t, s.cols, s.rows)));
        }
        let page_list: Vec<(String, u32, u32)> =
            pages.iter().map(|p| p.clone().unwrap_or_else(|| (String::new(), 1, 1))).collect();
        // A re-import of the same level reuses its tileset, keeping any
        // collision or tags changed since.
        let wanted = floptle_tiles::tileset_path(&level);
        let reuse = self.tiles.sets.get(&wanted).is_some_and(|set| {
            set.page_count() as usize == page_list.len()
                && page_list.iter().enumerate().all(|(i, (t, c, r))| set.page(i as u32) == Some((t.as_str(), *c, *r)))
        });
        let set_path = if reuse {
            wanted
        } else {
            let (t0, c0, r0) = page_list.first().cloned().unwrap_or_else(|| (String::new(), 1, 1));
            let p = self.new_tileset(&level, &t0, c0, r0);
            if let Some(set) = self.tiles.sets.get_mut(&p) {
                set.pages = page_list
                    .iter()
                    .skip(1)
                    .map(|(t, c, r)| floptle_tiles::TilePage { texture: t.clone(), cols: *c, rows: *r })
                    .collect();
                for (page, s) in lv.sheets.iter().enumerate() {
                    for (&cell, tags) in &s.tags {
                        let ix = floptle_core::tile_cell_of(page as u32, cell);
                        set.info_mut(ix).tags.extend(tags.iter().cloned());
                    }
                }
            }
            let cells: Vec<u32> = page_list
                .iter()
                .enumerate()
                .flat_map(|(p, (_, c, r))| (0..c * r).map(move |i| floptle_core::tile_cell_of(p as u32, i)))
                .collect();
            self.collision_from_art(&p, &cells);
            p
        };

        let origin = at.unwrap_or(DVec3::new(lv.origin.0 as f64 / ppu, -lv.origin.1 as f64 / ppu, 0.0));
        let root = self.world.spawn();
        self.world.insert(root, Transform { translation: origin, rotation: Quat::IDENTITY, scale: Vec3::ONE });
        self.world.insert(root, floptle_core::Name(level.clone()));
        self.world.insert(root, Matter::Empty);

        let mut colliding = Vec::new();
        for (i, l) in lv.layers.iter().enumerate() {
            let data: Vec<u32> = l
                .squares
                .iter()
                .map(|q| match q {
                    Some(q) if pages.get(q.sheet).is_some_and(Option::is_some) => {
                        floptle_core::tile_pack(floptle_core::tile_cell_of(q.sheet as u32, q.cell), q.xf)
                    }
                    _ => floptle_core::EMPTY_TILE,
                })
                .collect();
            let e = self.world.spawn();
            let local = DVec3::new(
                l.offset.0 as f64 / ppu + l.cols as f64 * 0.5,
                -(l.offset.1 as f64 / ppu + l.rows as f64 * 0.5),
                i as f64 * 0.1,
            );
            self.world.insert(e, Transform { translation: local, rotation: Quat::IDENTITY, scale: Vec3::ONE });
            self.world.insert(e, floptle_core::Name(l.name.clone()));
            self.world.insert(
                e,
                Matter::Tilemap { cols: l.cols, rows: l.rows, tile: 1.0, data, tileset: set_path.clone() },
            );
            self.world.insert(e, floptle_core::Material { unlit: true, ..Default::default() });
            self.world.insert(e, floptle_core::Parent(root));
            if l.visible {
                self.world.insert(e, floptle_core::Collidable);
                colliding.push(l.name.clone());
            } else {
                self.world.insert(e, floptle_core::Visible(false));
            }
        }
        let front = lv.layers.len() as f64 * 0.1;
        for o in &lv.objects {
            let e = self.world.spawn();
            let at = DVec3::new((o.x + o.w * 0.5) as f64 / ppu, -((o.y + o.h * 0.5) as f64) / ppu, front);
            self.world.insert(e, Transform { translation: at, rotation: Quat::IDENTITY, scale: Vec3::ONE });
            self.world.insert(e, floptle_core::Name(o.name.clone()));
            self.world.insert(e, Matter::Empty);
            self.world.insert(e, floptle_core::Tags(o.tags.clone()));
            self.world.insert(e, floptle_core::Parent(root));
        }
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!(
                "imported {level} from {stem}: {} layer(s) on {set_path}, {} object(s), {} pixels per unit. \
                 Collidable: {}. Untick Collidable on any layer that is only decoration.",
                lv.layers.len(),
                lv.objects.len(),
                lv.tile_w,
                if colliding.is_empty() { "none".to_string() } else { colliding.join(", ") }
            ),
            None,
        );
        Some(root)
    }
}

#[cfg(test)]
mod tests {
    /// A Tiled map in the project becomes a node with a tile layer per layer,
    /// an empty node per object, and a tileset whose solid tiles came from the
    /// art; turned tiles keep their turn.
    #[test]
    fn a_tiled_map_lands_in_the_scene() {
        let dir = std::env::temp_dir().join(format!("floptle_tmx_apply_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("levels")).unwrap();
        // A 2×2 sheet of 8-pixel tiles with a pixel of spacing: tile 0 solid,
        // the rest empty.
        let mut img = image::RgbaImage::new(17, 17);
        for y in 0..8 {
            for x in 0..8 {
                img.put_pixel(x, y, image::Rgba([200, 100, 50, 255]));
            }
        }
        img.save(dir.join("levels/ground.png")).unwrap();
        let tmx = r#"<map orientation="orthogonal" width="3" height="2" tilewidth="8" tileheight="8">
 <tileset firstgid="1" name="ground" tilewidth="8" tileheight="8" spacing="1" tilecount="4" columns="2">
  <image source="ground.png" width="17" height="17"/>
 </tileset>
 <layer name="Ground" width="3" height="2"><data encoding="csv">0,0,0,1,2684354561,1</data></layer>
 <objectgroup name="Spawns"><object name="Start" type="spawn" x="8" y="0" width="8" height="8"/></objectgroup>
</map>"#;
        std::fs::write(dir.join("levels/one.tmx"), tmx).unwrap();

        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        ed.import_level_file(&dir.join("levels/one.tmx").to_string_lossy(), None);

        let find = |ed: &crate::Editor, n: &str| {
            ed.world.query::<floptle_core::Name>().find(|(_, name)| name.0 == n).map(|(e, _)| e)
        };
        let ground = find(&ed, "Ground").expect("a tile layer named after the Tiled layer");
        let Some(floptle_core::Matter::Tilemap { cols, rows, data, tileset, .. }) =
            ed.world.get::<floptle_core::Matter>(ground).cloned()
        else {
            panic!("not a tilemap")
        };
        assert_eq!((cols, rows), (3, 2));
        assert_eq!(data[3], 0);
        assert_eq!(data[0], floptle_core::EMPTY_TILE);
        // gid 1 with the diagonal and horizontal flags: Tiled's clockwise turn.
        assert_eq!(floptle_core::tile_xform(data[4]), floptle_core::TileXform::new(1, false));
        let set = ed.tiles.sets.get(&tileset).expect("the tileset");
        assert!(set.collision(0).is_solid(), "tile 0 is solid by its art");
        assert!(!set.collision(1).is_solid(), "an empty tile is not");
        assert_eq!(set.texture, "levels/ground.grid.png", "spacing is taken out into a clean grid");
        assert!(dir.join("levels/ground.grid.png").exists());
        assert!(ed.world.get::<floptle_core::Collidable>(ground).is_some());

        // The bottom row's top edge is at y = -1, so the layer's centre sits
        // at (1.5, -1) under the level node; its bottom-left tile at x 0..1.
        let t = floptle_core::world_transform(&ed.world, ground).translation;
        assert_eq!((t.x, t.y), (1.5, -1.0));
        let start = find(&ed, "Start").expect("the object");
        let s = floptle_core::world_transform(&ed.world, start).translation;
        assert_eq!((s.x, s.y), (1.5, -0.5));
        assert!(ed.world.get::<floptle_core::Tags>(start).is_some_and(|t| t.0.contains(&"spawn".to_string())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Import `$LEVEL_FILE` into the first scene of the project at `$LEVEL_PROJECT`
/// and save it, to render with `floptle shot`.
#[cfg(test)]
#[test]
#[ignore]
fn probe_import_a_level() {
    let root = PathBuf::from(std::env::var("LEVEL_PROJECT").expect("LEVEL_PROJECT"));
    let file = std::env::var("LEVEL_FILE").expect("LEVEL_FILE");
    let mut ed = crate::Editor::default();
    ed.open_project(root.clone());
    ed.open_scene_file(&root.join("scenes/first.ron").to_string_lossy());
    ed.console.mirror_to_stderr = true;
    ed.import_level_file(&file, Some(DVec3::new(0.0, 8.0, 0.0)));
    assert!(ed.save_scene());
}
