//! The driver's half of `gpu.*` and `compute.run`: what scripts asked for,
//! made on the GPU, run before the frame is drawn, and answered.
//!
//! Each frame in Play the requests are taken in the order scripts made them:
//! buffers and textures are made, written and freed, shaders are loaded (and
//! loaded again when their file changes) and queued, and reads are started.
//! The queue runs at the start of the next draw, each job under its own GPU
//! timer mark, so anything drawn that frame sees its results. A texture a
//! shader writes is registered as `gpu:N`, drawable anywhere a texture name
//! goes. Everything is let go on Stop and on a scene load.

use std::collections::{HashMap, HashSet};

use floptle_render::compute::{BufferId, Bound, Compute, ComputeError, Job, TextureId};
use floptle_script::{ComputeBind, ComputeRequest};

use crate::Editor;

/// What the driver made for scripts, under the numbers scripts know them by.
#[derive(Default)]
pub(crate) struct ComputeJobs {
    compute: Compute,
    buffers: HashMap<u32, BufferId>,
    textures: HashMap<u32, (TextureId, floptle_render::TexId)>,
    /// Shader files by path: the time they were read and the text.
    sources: HashMap<String, (Option<floptle_core::time::SystemTime>, String)>,
    /// Errors from the last run, reported on the next pump.
    errors: Vec<ComputeError>,
    /// Lines said this Play, so a shader that fails every frame says so once.
    said: HashSet<String>,
    /// The device's limits went to the scripts, or why it has no compute.
    told: bool,
    unsupported: Option<String>,
}

/// The workgroup size an entry point declares, from its `@workgroup_size`.
fn workgroup_size(source: &str, entry: &str) -> Option<[u32; 3]> {
    let at = source.find(&format!("fn {entry}("))?;
    let attr = source[..at].rfind("@workgroup_size(")?;
    let open = attr + "@workgroup_size(".len();
    let close = open + source[open..].find(')')?;
    let mut out = [1u32; 3];
    for (i, part) in source[open..close].split(',').enumerate().take(3) {
        let n = part.trim().trim_end_matches(['u', 'i']);
        out[i] = n.parse().ok()?;
    }
    Some(out)
}

/// A shader file's text, read again when the file changes.
fn compute_source(root: &std::path::Path, jobs: &mut ComputeJobs, path: &str) -> Option<String> {
    let file = crate::project::resolve_asset_path(root, path);
    let stamp = floptle_vfs::modified(&file);
    if let Some((when, text)) = jobs.sources.get(path)
        && (stamp.is_none() || *when == stamp)
    {
        return Some(text.clone());
    }
    let text = floptle_vfs::read_to_string(&file).ok()?;
    jobs.sources.insert(path.to_string(), (stamp, text.clone()));
    Some(text)
}

impl Editor {
    /// Once a frame in Play: take what scripts asked for, and answer the reads
    /// the GPU has finished.
    pub(crate) fn pump_compute(&mut self) {
        let requests = self.script_host.take_compute_requests();
        let mut jobs = std::mem::take(&mut self.compute);
        if let Some(gpu) = self.gpu.as_ref() {
            if !jobs.told {
                jobs.told = true;
                match Compute::supported(gpu) {
                    Ok(()) => self.script_host.set_compute_limits(
                        Compute::limits(gpu).into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                    ),
                    Err(why) => jobs.unsupported = Some(why),
                }
            }
            for r in requests {
                // A write, read or free keeps its place after the shaders asked
                // for before it: those run first, or a read would see the buffer
                // as it was and a write would land under a shader meant to go
                // before it.
                if !matches!(r, ComputeRequest::Run { .. }) && jobs.compute.queued() > 0 {
                    let gpu = self.gpu.as_ref().expect("checked above");
                    let failed = jobs.compute.run_queued(gpu, self.play_t, &mut |_| {});
                    jobs.errors.extend(failed);
                }
                self.compute_request(&mut jobs, r);
            }
            let gpu = self.gpu.as_ref().expect("checked above");
            for (token, result) in jobs.compute.poll(gpu) {
                self.script_host.answer_compute_read(token, result);
            }
        } else {
            // No GPU in this host: nothing is made, and a read says why.
            for r in requests {
                if let ComputeRequest::ReadBuffer { token, .. } | ComputeRequest::ReadTexture { token, .. } = r {
                    self.script_host.answer_compute_read(token, Err(floptle_script::NO_RENDERER.into()));
                }
            }
        }
        for e in std::mem::take(&mut jobs.errors) {
            let line = e.to_string();
            if jobs.said.insert(line.clone()) {
                let source = Some((e.name.clone(), e.line.unwrap_or(1)));
                self.console.push(floptle_script::LogLevel::Error, format!("compute: {line}"), source);
            }
        }
        self.compute = jobs;
    }

    fn compute_request(&mut self, jobs: &mut ComputeJobs, r: ComputeRequest) {
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else { return };
        let fail = |jobs: &mut ComputeJobs, name: &str, message: String| {
            jobs.errors.push(ComputeError { name: name.to_string(), line: None, message });
        };
        if let Some(why) = jobs.unsupported.clone() {
            match r {
                ComputeRequest::ReadBuffer { token, .. } | ComputeRequest::ReadTexture { token, .. } => {
                    self.script_host.answer_compute_read(token, Err(why));
                }
                ComputeRequest::Run { path, .. } => fail(jobs, &path, why),
                _ => {}
            }
            return;
        }
        match r {
            ComputeRequest::Buffer { id, elem, count } => match jobs.compute.create_buffer(gpu, elem, count) {
                Ok(b) => {
                    jobs.buffers.insert(id, b);
                }
                Err(m) => fail(jobs, "gpu.buffer", m),
            },
            ComputeRequest::Write { id, first, bytes } => {
                if let Some(&b) = jobs.buffers.get(&id)
                    && let Err(m) = jobs.compute.write_buffer(gpu, b, first, &bytes)
                {
                    fail(jobs, "buffer:write", m);
                }
            }
            ComputeRequest::Texture { id, w, h } => match jobs.compute.create_texture(gpu, w, h) {
                Ok(t) => {
                    let texture = jobs.compute.gpu_texture(t).expect("just made").clone();
                    let tex = raster.register_gpu_texture(gpu, texture, Compute::srgb_views(gpu), Default::default());
                    self.texture_registry.insert(format!("gpu:{id}"), tex);
                    jobs.textures.insert(id, (t, tex));
                }
                Err(m) => fail(jobs, "gpu.texture", m),
            },
            ComputeRequest::FreeBuffer(id) => {
                if let Some(b) = jobs.buffers.remove(&id) {
                    jobs.compute.free_buffer(b);
                }
            }
            ComputeRequest::FreeTexture(id) => {
                if let Some((t, tex)) = jobs.textures.remove(&id) {
                    jobs.compute.free_texture(t);
                    raster.release_texture(gpu, tex);
                    self.texture_registry.remove(&format!("gpu:{id}"));
                }
            }
            ComputeRequest::ReadBuffer { id, token } => {
                let answer = match jobs.buffers.get(&id) {
                    Some(&b) => jobs.compute.read_buffer(gpu, b, token),
                    None => Err("that buffer has been freed".into()),
                };
                if let Err(m) = answer {
                    self.script_host.answer_compute_read(token, Err(m));
                }
            }
            ComputeRequest::ReadTexture { id, token } => {
                let answer = match jobs.textures.get(&id) {
                    Some(&(t, _)) => jobs.compute.read_texture(gpu, t, token),
                    None => Err("that texture has been freed".into()),
                };
                if let Err(m) = answer {
                    self.script_host.answer_compute_read(token, Err(m));
                }
            }
            ComputeRequest::Run { path, entry, groups, size, bind, params } => {
                let Some(source) = compute_source(&self.project_root, jobs, &path) else {
                    fail(jobs, &path, "no shader file there".into());
                    return;
                };
                let groups = match (groups, size) {
                    (Some(g), _) => g,
                    (None, Some(size)) => match workgroup_size(&source, &entry) {
                        Some(wg) => [0, 1, 2].map(|i| size[i].div_ceil(wg[i].max(1))),
                        None => {
                            fail(
                                jobs,
                                &path,
                                format!("`{entry}` has no @workgroup_size of plain numbers to divide `size` by: give `groups`"),
                            );
                            return;
                        }
                    },
                    (None, None) => [1, 1, 1],
                };
                let mut bound = Vec::with_capacity(bind.len());
                for (name, b) in bind {
                    let found = match b {
                        ComputeBind::Buffer(id) => jobs.buffers.get(&id).map(|&b| Bound::Buffer(b)),
                        ComputeBind::Write(id) => jobs.textures.get(&id).map(|&(t, _)| Bound::WriteTexture(t)),
                        ComputeBind::Read(id) => jobs.textures.get(&id).map(|&(t, _)| Bound::ReadTexture(t)),
                    };
                    match found {
                        Some(b) => bound.push((name, b)),
                        None => {
                            fail(jobs, &path, format!("`{name}` was freed before the shader ran"));
                            return;
                        }
                    }
                }
                jobs.compute.queue(Job { name: path, source, entry, groups, bind: bound, params });
            }
        }
    }

    /// Run what scripts queued, before this frame draws. `timing` marks each
    /// job for the GPU timer.
    pub(crate) fn run_compute_jobs(&mut self, timing: bool) {
        if self.compute.compute.queued() == 0 {
            return;
        }
        let Some(gpu) = self.gpu.as_ref() else { return };
        let time = self.play_t;
        let mut timer = self.gpu_timer.as_mut().filter(|_| timing);
        let failed = self.compute.compute.run_queued(gpu, time, &mut |label| {
            if let Some(t) = timer.as_mut() {
                t.mark(gpu, label);
            }
        });
        self.compute.errors.extend(failed);
    }

    /// Stop, or a scene load: let go of everything scripts made on the GPU.
    pub(crate) fn reset_compute(&mut self) {
        let jobs = std::mem::take(&mut self.compute);
        if let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) {
            for (id, (_, tex)) in jobs.textures {
                raster.release_texture(gpu, tex);
                self.texture_registry.remove(&format!("gpu:{id}"));
            }
        }
        // The device's answer does not change between Plays.
        self.compute.told = jobs.told;
        self.compute.unsupported = jobs.unsupported;
    }
}

#[cfg(test)]
mod tests {
    use super::workgroup_size;

    /// **A script's compute shader runs, and its results come back.** A
    /// buffer written from Lua and doubled by a shader read from the project
    /// reads back changed, a texture it paints reads back painted and is
    /// drawable as `gpu:1`, and Stop lets both go.
    #[test]
    fn a_script_runs_a_compute_shader_and_reads_what_it_wrote() {
        use floptle_core::{Matter, Name, ScriptInst, Scripts, Transform};
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        if let Err(why) = ed.gpu.as_ref().map_or(Ok(()), floptle_render::compute::Compute::supported) {
            eprintln!("skipped — {why}");
            return;
        }
        // What the GPU refused along the way: a texture that could not be
        // viewed for drawing still answered its reads, so the results alone
        // passed with the picture broken.
        let refused = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = refused.clone();
        ed.gpu.as_ref().unwrap().device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
            if let Ok(mut v) = sink.lock() {
                v.push(e.to_string());
            }
        }));
        let dir = std::env::temp_dir().join(format!("floptle-compute-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shaders")).unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(
            dir.join("shaders/double.wgsl"),
            "@compute @workgroup_size(64)\nfn double(@builtin(global_invocation_id) id: vec3<u32>) {\n  \
             cells[id.x] = cells[id.x] * 2u + u32(params.add);\n  \
             textureStore(skin, vec2<i32>(i32(id.x % 16u), i32(id.x / 16u)), vec4<f32>(1.0, 0.0, 0.0, 1.0));\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("scripts/driver.lua"),
            r#"
local frame, buf, tex = 0, nil, nil
function update(node, dt)
  frame = frame + 1
  if frame == 1 then
    buf = gpu.buffer{ count = 256, type = "u32" }
    tex = gpu.texture{ w = 16, h = 16 }
    buf:write({ 5, 6, 7 }, 0)
    compute.run("shaders/double.wgsl", "double", { size = { 256 }, bind = { cells = buf, skin = tex }, params = { add = 1 } })
    log("CMP name " .. tex.name)
  elseif frame == 3 then
    buf:read(function(v, err) log("CMP buf " .. tostring(err) .. " " .. v[1] .. " " .. v[2] .. " " .. v[3] .. " " .. v[256]) end)
    tex:read(function(b, err) log("CMP tex " .. tostring(err) .. " " .. #b .. " " .. string.byte(b, 1) .. " " .. string.byte(b, 2) .. " " .. string.byte(b, 4)) end)
  end
end
"#,
        )
        .unwrap();
        ed.project_root = dir.clone();
        let driver = ed.world.spawn();
        ed.world.insert(driver, Transform::IDENTITY);
        ed.world.insert(driver, Name("Driver".into()));
        ed.world.insert(driver, Matter::Empty);
        ed.world.insert(
            driver,
            Scripts(vec![ScriptInst {
                kind: "driver".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        ed.toggle_play();
        let mut registered = false;
        for _ in 0..120 {
            ed.play_step(1.0 / 60.0, true);
            ed.run_compute_jobs(false);
            registered |= ed.texture_registry.contains_key("gpu:1");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        ed.drain_script_logs();
        let lines: Vec<String> = ed.console.entries.iter().filter_map(|e| e.msg.strip_prefix("CMP ").map(str::to_string)).collect();
        let problems: Vec<String> = ed.console.entries.iter().filter(|e| e.msg.starts_with("compute:")).map(|e| e.msg.clone()).collect();
        ed.toggle_play();
        let released = !ed.texture_registry.contains_key("gpu:1");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(problems.is_empty(), "{problems:?}");
        let refused = refused.lock().map(|v| v.clone()).unwrap_or_default();
        assert!(refused.is_empty(), "the GPU refused something: {refused:?}");
        assert_eq!(
            lines,
            vec!["name gpu:1".to_string(), "buf nil 11 13 15 1".to_string(), "tex nil 1024 255 0 255".to_string()],
            "the shader's results did not come back"
        );
        assert!(registered, "the texture was drawable as gpu:1");
        assert!(released, "Stop let the texture go");
    }

    /// **A read made after a run sees what the run wrote, render or no
    /// render.** A script writes, runs and reads in one frame; the frames go
    /// by with no draw between them, as a headless shot plays. The read used to
    /// be copied out at once while the run waited for the next draw, and came
    /// back as the buffer was before.
    #[test]
    fn a_read_after_a_run_sees_the_run_without_a_draw_between() {
        use floptle_core::{Matter, Name, ScriptInst, Scripts, Transform};
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        if let Err(why) = ed.gpu.as_ref().map_or(Ok(()), floptle_render::compute::Compute::supported) {
            eprintln!("skipped — {why}");
            return;
        }
        let dir = std::env::temp_dir().join(format!("floptle-compute-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shaders")).unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(
            dir.join("shaders/add.wgsl"),
            "@compute @workgroup_size(4)\nfn add(@builtin(global_invocation_id) id: vec3<u32>) {\n  v[id.x] = v[id.x] + 10u;\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("scripts/driver.lua"),
            r#"
local frame = 0
function update(node, dt)
  frame = frame + 1
  if frame == 1 then
    local v = gpu.buffer{ count = 4, type = "u32" }
    v:write({ 1, 2, 3, 4 })
    compute.run("shaders/add.wgsl", "add", { groups = { 1 }, bind = { v = v } })
    v:read(function(got) log("ORD " .. got[1] .. " " .. got[4]) end)
    v:write({ 7 })
  end
end
"#,
        )
        .unwrap();
        ed.project_root = dir.clone();
        let driver = ed.world.spawn();
        ed.world.insert(driver, Transform::IDENTITY);
        ed.world.insert(driver, Name("Driver".into()));
        ed.world.insert(driver, Matter::Empty);
        ed.world.insert(
            driver,
            Scripts(vec![ScriptInst {
                kind: "driver".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        ed.toggle_play();
        for _ in 0..60 {
            ed.play_step(1.0 / 60.0, true);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        ed.drain_script_logs();
        let lines: Vec<String> = ed.console.entries.iter().filter_map(|e| e.msg.strip_prefix("ORD ").map(str::to_string)).collect();
        let _ = std::fs::remove_dir_all(&dir);
        // The read is after the run and before the second write.
        assert_eq!(lines, vec!["11 14".to_string()], "the read did not see the run, in order");
    }

    #[test]
    fn the_workgroup_size_is_read_from_the_entry_point() {
        let src = "@compute @workgroup_size(8, 8)\nfn a(@builtin(global_invocation_id) id: vec3<u32>) {}\n\
                   @compute @workgroup_size(64u)\nfn b(@builtin(global_invocation_id) id: vec3<u32>) {}\n";
        assert_eq!(workgroup_size(src, "a"), Some([8, 8, 1]));
        assert_eq!(workgroup_size(src, "b"), Some([64, 1, 1]));
        assert_eq!(workgroup_size(src, "c"), None);
        assert_eq!(workgroup_size("@compute @workgroup_size(N)\nfn d() {}", "d"), None, "a constant is not guessed");
    }
}
