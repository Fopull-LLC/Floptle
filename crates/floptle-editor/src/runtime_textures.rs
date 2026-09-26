//! Pictures a game gets while it runs (`assets.textureFromUrl`,
//! `assets.textureFromBytes`), made into textures every drawing path can use.
//!
//! The script host hands over bytes and a name (`img:<n>`). The bytes are
//! decoded on a worker by [`floptle_assets::decode_untrusted`] — PNG, JPEG or
//! WebP by signature, within a size limit, pixels and nothing else — and the
//! pixels are uploaded here, on the main thread, into the one texture registry
//! that `draw.quad`, UI images and materials all read. The name is a registry
//! key that names no file, like a render target's `rt:`, so nothing ever tries
//! to load it from disk.

use std::sync::mpsc::{Receiver, TryRecvError};

use floptle_render::TextureData;

use crate::Editor;

/// A registry key that is not a path: a live render target (`rt:`) or a
/// runtime picture (`img:`). Looked up, never loaded, never hot-reloaded.
pub(crate) fn names_no_file(key: &str) -> bool {
    key.starts_with("rt:") || key.starts_with("img:")
}

/// A picture being decoded.
pub(crate) struct TextureJob {
    id: u64,
    name: String,
    rx: Receiver<Result<TextureData, String>>,
}

impl Editor {
    /// Once a frame in Play, in every host: start decoding what scripts handed
    /// over, upload what has finished, answer, and let go of what they
    /// released.
    pub(crate) fn pump_runtime_textures(&mut self) {
        let can_draw = self.gpu.is_some() && self.raster.is_some();
        self.script_host.set_can_draw(can_draw);
        for name in self.script_host.take_texture_releases() {
            self.release_runtime_texture(&name);
        }
        for req in self.script_host.take_texture_requests() {
            let Some(gpu) = self.gpu.as_ref().filter(|_| can_draw) else {
                self.script_host.answer_texture(req.id, Err("this host draws nothing, so it makes no textures".into()));
                continue;
            };
            // Past the GPU's own limit a texture cannot be created at all.
            let max_side = gpu.device.limits().max_texture_dimension_2d;
            let (tx, rx) = std::sync::mpsc::channel();
            let bytes = req.bytes;
            crate::worker::spawn("floptle-texture-decode", move || {
                let _ = tx.send(floptle_assets::decode_untrusted(&bytes, max_side));
            });
            self.texture_jobs.push(TextureJob { id: req.id, name: req.name, rx });
        }
        if self.texture_jobs.is_empty() {
            return;
        }
        let mut done = Vec::new();
        self.texture_jobs.retain(|j| match j.rx.try_recv() {
            Ok(r) => {
                done.push((j.id, j.name.clone(), r));
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => {
                done.push((j.id, j.name.clone(), Err("the decode worker stopped".into())));
                false
            }
        });
        for (id, name, decoded) in done {
            let answer = decoded.and_then(|data| {
                let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                    return Err("this host draws nothing, so it makes no textures".into());
                };
                let tex = raster.register_texture(gpu, &data, Default::default());
                self.texture_registry.insert(name.clone(), tex);
                Ok(name)
            });
            self.script_host.answer_texture(id, answer);
        }
    }

    fn release_runtime_texture(&mut self, name: &str) {
        let Some(tex) = self.texture_registry.remove(name) else { return };
        if let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) {
            raster.release_texture(gpu, tex);
        }
    }

    /// Stop: every runtime picture goes with the session that asked for it.
    pub(crate) fn release_runtime_textures(&mut self) {
        self.texture_jobs.clear();
        let names: Vec<String> =
            self.texture_registry.keys().filter(|k| k.starts_with("img:")).cloned().collect();
        for n in names {
            self.release_runtime_texture(&n);
        }
    }
}

#[cfg(test)]
mod tests {
    use floptle_core::{ScriptInst, Scripts, Transform, World};

    /// A 3×2 PNG as a Lua string literal, every byte escaped.
    fn png_literal() -> String {
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([200, 30, 40, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner().iter().map(|b| format!("\\{b}")).collect()
    }

    /// A world whose one script asks for a texture from bytes in `start`.
    fn scene(tag: &str) -> (World, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("floptle-runtime-tex-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("avatar.lua"),
            format!(
                "function start(node)\n  assets.textureFromBytes(\"{}\", function(t, e) tex = t; err = e end)\nend\n",
                png_literal()
            ),
        )
        .unwrap();
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![ScriptInst {
                kind: "avatar".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        (world, dir)
    }

    fn frames(ed: &mut crate::Editor, world: &mut World, dir: &std::path::Path, until: impl Fn(&crate::Editor) -> bool) {
        for i in 0..500 {
            ed.script_host.run(world, dir, 1.0 / 60.0, i as f32 / 60.0);
            ed.pump_runtime_textures();
            if until(ed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn img_keys(ed: &crate::Editor) -> Vec<String> {
        ed.texture_registry.keys().filter(|k| k.starts_with("img:")).cloned().collect()
    }

    /// Bytes a script hands over come out as a texture every drawing path can
    /// name: registered under `img:<n>`, found by the lookup `draw.quad`, UI
    /// images and materials share, and never looked for on disk. Stop lets go.
    #[test]
    fn bytes_from_a_script_become_a_texture_every_drawing_path_can_name() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let (mut world, dir) = scene("gpu");
        frames(&mut ed, &mut world, &dir, |ed| !img_keys(ed).is_empty());
        let keys = img_keys(&ed);
        assert_eq!(keys.len(), 1, "no texture was made: {keys:?}");
        let name = keys[0].clone();

        let tex = ed.ensure_texture(&name).expect("the shared lookup does not find it");
        let size = ed.raster.as_ref().unwrap().texture_size(tex);
        assert_eq!(size, Some([3.0, 2.0]));
        // A file-backed lookup would have missed and dropped it: it is still there.
        assert_eq!(ed.ensure_texture(&name), Some(tex));

        ed.release_runtime_textures();
        assert!(img_keys(&ed).is_empty(), "Stop kept a runtime texture");
        assert_eq!(ed.ensure_texture(&name), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No GPU: the script is told why, and nothing is decoded.
    #[test]
    fn a_host_with_no_gpu_answers_the_script() {
        let mut ed = crate::Editor::default();
        let (mut world, dir) = scene("nogpu");
        let e = world.query::<Scripts>().next().map(|(e, _)| e).unwrap();
        let answered = |ed: &crate::Editor| {
            ed.script_host.instance_env(e.index(), "avatar").is_some_and(|env| env.get::<Option<String>>("err").ok().flatten().is_some())
        };
        frames(&mut ed, &mut world, &dir, answered);
        assert!(answered(&ed), "the script was never answered");
        assert!(ed.texture_jobs.is_empty(), "decoded with nothing to upload to");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
