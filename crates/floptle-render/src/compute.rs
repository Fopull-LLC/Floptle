//! Compute: GPU programs that draw nothing.
//!
//! A compute shader reads and writes buffers and textures, thousands of
//! invocations at once, and makes no picture of its own: a procedural texture,
//! a particle swarm, a simulation, a mesh painted on the GPU. This module is
//! the renderer's half: the buffers and textures a game makes, the shaders
//! compiled against them, the jobs queued during a frame and run in order
//! before the frame is drawn, and results read back without stalling.
//!
//! A shader is WGSL with `@compute` entry points. The engine writes the
//! declarations in front of it (one named global per binding, so nobody
//! counts binding numbers, plus `params` with `time` and `frame`) and FLSL's
//! noise and colour library, and reports an error at the shader's own line.
//!
//! What runs everywhere the engine ships (desktop, the browser's WebGPU, and
//! OpenGL 4.3+) is the baseline: a texture a shader writes is `rgba8unorm`
//! and write-only, and one a shader reads is bound for reading; to update a
//! picture in place, write a second one and swap (ping-pong).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::Gpu;

pub use floptle_core::gpu::Elem;

fn wgsl(e: &Elem, struct_name: &str) -> String {
    match e {
        Elem::F32 => "f32".into(),
        Elem::U32 => "u32".into(),
        Elem::I32 => "i32".into(),
        Elem::AtomicU32 => "atomic<u32>".into(),
        Elem::Vec2 => "vec2<f32>".into(),
        Elem::Vec3 => "vec3<f32>".into(),
        Elem::Vec4 => "vec4<f32>".into(),
        Elem::Struct(_) => struct_name.into(),
    }
}

/// A buffer the compute side made, by number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufferId(pub u32);

/// A texture the compute side made, by number. Drawable anywhere a texture
/// goes once the renderer has registered it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextureId(pub u32);

struct Buffer {
    buffer: wgpu::Buffer,
    elem: Elem,
    count: u32,
}

struct Texture {
    texture: wgpu::Texture,
    width: u32,
    height: u32,
}

/// What a shader binds, under a name the shader uses.
#[derive(Clone, Debug, PartialEq)]
pub enum Bound {
    Buffer(BufferId),
    /// A texture the shader writes (`texture_storage_2d<rgba8unorm, write>`).
    WriteTexture(TextureId),
    /// A texture the shader reads (`texture_2d<f32>`, `textureLoad`).
    ReadTexture(TextureId),
}

/// One dispatch, as a script asked for it.
#[derive(Clone, Debug)]
pub struct Job {
    /// Where the source came from, for errors and for the GPU timer.
    pub name: String,
    pub source: String,
    pub entry: String,
    /// Workgroups in x, y, z.
    pub groups: [u32; 3],
    /// Bindings by the name the shader uses, in a fixed order.
    pub bind: Vec<(String, Bound)>,
    /// Uniforms, by name, alongside `time` and `frame`.
    pub params: Vec<(String, f32)>,
}

/// Why a job did not run: what was wrong, and where in the shader when the
/// compiler said.
#[derive(Clone, Debug, PartialEq)]
pub struct ComputeError {
    pub name: String,
    /// 1-based line in the shader's own source.
    pub line: Option<u32>,
    pub message: String,
}

impl std::fmt::Display for ComputeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(l) => write!(f, "{}:{l}: {}", self.name, self.message),
            None => write!(f, "{}: {}", self.name, self.message),
        }
    }
}

struct Pipeline {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

/// A copy back to the CPU, waiting for the GPU to finish it.
struct Readback {
    token: u64,
    staging: wgpu::Buffer,
    state: Arc<AtomicU8>,
    /// For a texture: the padded row length to strip, the row length, rows.
    rows: Option<(u32, u32, u32)>,
}

/// The renderer's compute side. See the module docs.
#[derive(Default)]
pub struct Compute {
    buffers: Vec<Option<Buffer>>,
    textures: Vec<Option<Texture>>,
    pipelines: HashMap<u64, Pipeline>,
    queued: Vec<Job>,
    readbacks: Vec<Readback>,
    frame: u32,
}

/// The identifiers the engine declares, which a binding or uniform cannot be
/// named.
const RESERVED: &[&str] = &["params", "time", "frame"];

fn valid_name(n: &str) -> bool {
    let mut c = n.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && !RESERVED.contains(&n)
}

impl Compute {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this device runs compute shaders, and if not, why.
    pub fn supported(gpu: &Gpu) -> Result<(), String> {
        let flags = gpu.adapter.get_downlevel_capabilities().flags;
        if !flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS) {
            return Err("this graphics device has no compute shaders".into());
        }
        Ok(())
    }

    /// The device's limits that decide what a game can ask for, as
    /// `(name, value)` pairs.
    pub fn limits(gpu: &Gpu) -> Vec<(&'static str, u64)> {
        let l = gpu.device.limits();
        vec![
            ("maxBufferBytes", l.max_storage_buffer_binding_size),
            ("maxStorageBuffers", l.max_storage_buffers_per_shader_stage as u64),
            ("maxStorageTextures", l.max_storage_textures_per_shader_stage as u64),
            ("maxWorkgroups", l.max_compute_workgroups_per_dimension as u64),
            ("maxWorkgroupSize", l.max_compute_invocations_per_workgroup as u64),
            ("maxWorkgroupMemory", l.max_compute_workgroup_storage_size as u64),
            ("maxTextureSize", l.max_texture_dimension_2d as u64),
        ]
    }

    /// Make a buffer of `count` elements of `elem`, zeroed.
    pub fn create_buffer(&mut self, gpu: &Gpu, elem: Elem, count: u32) -> Result<BufferId, String> {
        if count == 0 {
            return Err("a buffer needs at least one element".into());
        }
        let bytes = elem.stride() as u64 * count as u64;
        let max = gpu.device.limits().max_storage_buffer_binding_size;
        if bytes > max {
            return Err(format!("{bytes} bytes is more than this device binds in one buffer ({max})"));
        }
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("compute buffer"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let id = BufferId(self.buffers.len() as u32);
        self.buffers.push(Some(Buffer { buffer, elem, count }));
        Ok(id)
    }

    /// Write `bytes` into a buffer from element `first` on.
    pub fn write_buffer(&self, gpu: &Gpu, id: BufferId, first: u32, bytes: &[u8]) -> Result<(), String> {
        let b = self.buffer(id)?;
        let at = first as u64 * b.elem.stride() as u64;
        let size = b.elem.stride() as u64 * b.count as u64;
        if at + bytes.len() as u64 > size {
            return Err(format!("{} bytes from element {first} run past the buffer's {size}", bytes.len()));
        }
        if !bytes.len().is_multiple_of(4) {
            return Err("a write is whole 4-byte values".into());
        }
        gpu.queue.write_buffer(&b.buffer, at, bytes);
        Ok(())
    }

    /// The element type and count of a buffer.
    pub fn buffer_shape(&self, id: BufferId) -> Option<(&Elem, u32)> {
        self.buffers.get(id.0 as usize)?.as_ref().map(|b| (&b.elem, b.count))
    }

    fn buffer(&self, id: BufferId) -> Result<&Buffer, String> {
        self.buffers
            .get(id.0 as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| format!("buffer {} has been freed", id.0))
    }

    fn texture(&self, id: TextureId) -> Result<&Texture, String> {
        self.textures
            .get(id.0 as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| format!("texture {} has been freed", id.0))
    }

    /// Make a `width`×`height` RGBA texture a shader can write, transparent
    /// black.
    pub fn create_texture(&mut self, gpu: &Gpu, width: u32, height: u32) -> Result<TextureId, String> {
        let max = gpu.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return Err(format!("{width}×{height} is outside what this device makes (1 to {max} a side)"));
        }
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("compute texture"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            // Drawn through an sRGB view where the device allows one, so a
            // shader's output reads as a picture's bytes do.
            view_formats: if Self::srgb_views(gpu) { &[wgpu::TextureFormat::Rgba8UnormSrgb] } else { &[] },
        });
        let id = TextureId(self.textures.len() as u32);
        self.textures.push(Some(Texture { texture, width, height }));
        Ok(id)
    }

    /// Whether a compute texture can be drawn through an sRGB view here.
    pub fn srgb_views(gpu: &Gpu) -> bool {
        gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS)
    }

    /// The GPU texture behind a compute texture, for the renderer to draw.
    pub fn gpu_texture(&self, id: TextureId) -> Option<&wgpu::Texture> {
        self.texture(id).ok().map(|t| &t.texture)
    }

    pub fn texture_size(&self, id: TextureId) -> Option<(u32, u32)> {
        self.texture(id).ok().map(|t| (t.width, t.height))
    }

    pub fn free_buffer(&mut self, id: BufferId) {
        if let Some(slot) = self.buffers.get_mut(id.0 as usize) {
            *slot = None;
        }
    }

    pub fn free_texture(&mut self, id: TextureId) {
        if let Some(slot) = self.textures.get_mut(id.0 as usize) {
            *slot = None;
        }
    }

    /// Free everything: a Stop, a scene load. Numbers are not reused, so a
    /// name kept from before reaches nothing.
    pub fn clear(&mut self) {
        for b in &mut self.buffers {
            *b = None;
        }
        for t in &mut self.textures {
            *t = None;
        }
        self.queued.clear();
        self.readbacks.clear();
    }

    /// Queue a job to run before the next frame is drawn.
    pub fn queue(&mut self, job: Job) {
        self.queued.push(job);
    }

    /// How many jobs wait for the next [`run_queued`](Self::run_queued).
    pub fn queued(&self) -> usize {
        self.queued.len()
    }

    /// The declarations written in front of a shader for these bindings and
    /// uniforms, and how many lines they take.
    fn prelude(&self, job: &Job) -> Result<(String, u32), String> {
        let mut out = String::new();
        out.push_str("struct FloptleParams {\n  time: f32,\n  frame: u32,\n");
        for (n, _) in &job.params {
            if !valid_name(n) {
                return Err(format!("`{n}` is not a name a uniform can have"));
            }
            out.push_str(&format!("  {n}: f32,\n"));
        }
        out.push_str("}\n@group(0) @binding(0) var<uniform> params: FloptleParams;\n");
        for (i, (name, b)) in job.bind.iter().enumerate() {
            if !valid_name(name) {
                return Err(format!("`{name}` is not a name a binding can have"));
            }
            let at = i + 1;
            match b {
                Bound::Buffer(id) => {
                    let buf = self.buffer(*id)?;
                    let ty = format!("{name}_t");
                    if let Elem::Struct(fields) = &buf.elem {
                        out.push_str(&format!("struct {ty} {{\n"));
                        for (f, e) in fields {
                            out.push_str(&format!("  {f}: {},\n", wgsl(e, "")));
                        }
                        out.push_str("}\n");
                    }
                    out.push_str(&format!(
                        "@group(0) @binding({at}) var<storage, read_write> {name}: array<{}>;\n",
                        wgsl(&buf.elem, &ty)
                    ));
                }
                Bound::WriteTexture(id) => {
                    self.texture(*id)?;
                    out.push_str(&format!(
                        "@group(0) @binding({at}) var {name}: texture_storage_2d<rgba8unorm, write>;\n"
                    ));
                }
                Bound::ReadTexture(id) => {
                    self.texture(*id)?;
                    out.push_str(&format!("@group(0) @binding({at}) var {name}: texture_2d<f32>;\n"));
                }
            }
        }
        out.push_str(support_library());
        out.push('\n');
        let lines = out.matches('\n').count() as u32;
        Ok((out, lines))
    }

    /// The shape of a job's bindings, which is what its pipeline is built for.
    fn signature(&self, job: &Job) -> Result<u64, String> {
        let mut h = std::hash::DefaultHasher::new();
        job.source.hash(&mut h);
        job.entry.hash(&mut h);
        for (n, _) in &job.params {
            n.hash(&mut h);
        }
        for (n, b) in &job.bind {
            n.hash(&mut h);
            match b {
                Bound::Buffer(id) => {
                    0u8.hash(&mut h);
                    format!("{:?}", self.buffer(*id)?.elem).hash(&mut h);
                }
                Bound::WriteTexture(_) => 1u8.hash(&mut h),
                Bound::ReadTexture(_) => 2u8.hash(&mut h),
            }
        }
        Ok(h.finish())
    }

    fn pipeline_for(&mut self, gpu: &Gpu, job: &Job) -> Result<u64, ComputeError> {
        let err = |line: Option<u32>, message: String| ComputeError { name: job.name.clone(), line, message };
        let key = self.signature(job).map_err(|m| err(None, m))?;
        if self.pipelines.contains_key(&key) {
            return Ok(key);
        }
        let (prelude, lines) = self.prelude(job).map_err(|m| err(None, m))?;
        let Some((body, entry)) = entry_module(&job.source, &job.entry) else {
            return Err(err(None, format!("no @compute entry point called `{}`", job.entry)));
        };
        let src = format!("{prelude}{body}");
        // naga first: the error names the shader's own line, and a shader the
        // driver would have refused never reaches it.
        if let Err(d) = floptle_shader::transpile::validate_module(&src) {
            let line = d.chunk_line.and_then(|l| l.checked_sub(lines));
            return Err(err(line, d.message));
        }
        let module = gpu.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(&job.name),
            source: wgpu::ShaderSource::Wgsl(src.into()),
        });
        let mut entries = vec![wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }];
        for (i, (_, b)) in job.bind.iter().enumerate() {
            let ty = match b {
                Bound::Buffer(_) => wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                Bound::WriteTexture(_) => wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                Bound::ReadTexture(_) => wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
            };
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: i as u32 + 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty,
                count: None,
            });
        }
        let layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("compute bindings"),
            entries: &entries,
        });
        let pl = gpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("compute layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pipeline = gpu.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(&job.name),
            layout: Some(&pl),
            module: &module,
            entry_point: Some(&entry),
            compilation_options: Default::default(),
            cache: None,
        });
        // What the driver refused after naga passed it. A page cannot wait
        // for the answer; there a refused pipeline fails on use instead.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(e) = pollster::block_on(scope.pop()) {
            return Err(err(None, e.to_string()));
        }
        #[cfg(target_arch = "wasm32")]
        drop(scope.pop());
        self.pipelines.insert(key, Pipeline { pipeline, layout });
        Ok(key)
    }

    /// Run every queued job, in the order asked, each in its own submission so
    /// the GPU timer can name it. `mark` is called with each job's label just
    /// before it goes to the GPU. Returns the jobs that could not run.
    pub fn run_queued(&mut self, gpu: &Gpu, time: f32, mark: &mut dyn FnMut(&str)) -> Vec<ComputeError> {
        let jobs = std::mem::take(&mut self.queued);
        let mut failed = Vec::new();
        let max_groups = gpu.device.limits().max_compute_workgroups_per_dimension;
        for job in jobs {
            if job.groups.iter().any(|&g| g > max_groups) {
                failed.push(ComputeError {
                    name: job.name.clone(),
                    line: None,
                    message: format!("{:?} workgroups is more than this device runs ({max_groups} a side)", job.groups),
                });
                continue;
            }
            if job.groups.contains(&0) {
                continue;
            }
            let key = match self.pipeline_for(gpu, &job) {
                Ok(k) => k,
                Err(e) => {
                    failed.push(e);
                    continue;
                }
            };
            let mut uniform: Vec<u8> = Vec::with_capacity(16 + job.params.len() * 4);
            uniform.extend_from_slice(&time.to_le_bytes());
            uniform.extend_from_slice(&self.frame.to_le_bytes());
            for (_, v) in &job.params {
                uniform.extend_from_slice(&v.to_le_bytes());
            }
            uniform.resize(uniform.len().div_ceil(16) * 16, 0);
            let ubuf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("compute params"),
                size: uniform.len() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.queue.write_buffer(&ubuf, 0, &uniform);
            let views: Vec<Option<wgpu::TextureView>> = job
                .bind
                .iter()
                .map(|(_, b)| match b {
                    Bound::WriteTexture(id) | Bound::ReadTexture(id) => self
                        .texture(*id)
                        .ok()
                        .map(|t| t.texture.create_view(&wgpu::TextureViewDescriptor::default())),
                    Bound::Buffer(_) => None,
                })
                .collect();
            let mut entries = vec![wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() }];
            let mut missing = None;
            for (i, (name, b)) in job.bind.iter().enumerate() {
                let resource = match b {
                    Bound::Buffer(id) => match self.buffer(*id) {
                        Ok(buf) => buf.buffer.as_entire_binding(),
                        Err(m) => {
                            missing = Some(format!("`{name}`: {m}"));
                            break;
                        }
                    },
                    _ => match &views[i] {
                        Some(v) => wgpu::BindingResource::TextureView(v),
                        None => {
                            missing = Some(format!("`{name}`: that texture has been freed"));
                            break;
                        }
                    },
                };
                entries.push(wgpu::BindGroupEntry { binding: i as u32 + 1, resource });
            }
            if let Some(message) = missing {
                failed.push(ComputeError { name: job.name.clone(), line: None, message });
                continue;
            }
            let p = &self.pipelines[&key];
            let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("compute bindings"),
                layout: &p.layout,
                entries: &entries,
            });
            mark(&format!("compute {}", job.entry));
            let mut enc = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("compute") });
            {
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some(&job.name), timestamp_writes: None });
                pass.set_pipeline(&p.pipeline);
                pass.set_bind_group(0, &group, &[]);
                pass.dispatch_workgroups(job.groups[0], job.groups[1], job.groups[2]);
            }
            gpu.queue.submit(Some(enc.finish()));
        }
        self.frame = self.frame.wrapping_add(1);
        failed
    }

    /// Ask for a buffer's bytes back. They arrive from [`poll`](Self::poll)
    /// under `token`, a frame or two later.
    pub fn read_buffer(&mut self, gpu: &Gpu, id: BufferId, token: u64) -> Result<(), String> {
        let b = self.buffer(id)?;
        let size = b.elem.stride() as u64 * b.count as u64;
        let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("compute readback"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("compute readback") });
        enc.copy_buffer_to_buffer(&b.buffer, 0, &staging, 0, size);
        gpu.queue.submit(Some(enc.finish()));
        self.map(staging, token, None);
        Ok(())
    }

    /// Ask for a texture's pixels back, RGBA rows top to bottom.
    pub fn read_texture(&mut self, gpu: &Gpu, id: TextureId, token: u64) -> Result<(), String> {
        let t = self.texture(id)?;
        let row = t.width * 4;
        let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("compute readback"),
            size: padded as u64 * t.height as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("compute readback") });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &t.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(t.height) },
            },
            wgpu::Extent3d { width: t.width, height: t.height, depth_or_array_layers: 1 },
        );
        gpu.queue.submit(Some(enc.finish()));
        let rows = Some((padded, row, t.height));
        self.map(staging, token, rows);
        Ok(())
    }

    fn map(&mut self, staging: wgpu::Buffer, token: u64, rows: Option<(u32, u32, u32)>) {
        let state = Arc::new(AtomicU8::new(0));
        let s = state.clone();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            s.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release);
        });
        self.readbacks.push(Readback { token, staging, state, rows });
    }

    /// Readbacks the GPU has finished, by token. Never waits.
    pub fn poll(&mut self, gpu: &Gpu) -> Vec<(u64, Result<Vec<u8>, String>)> {
        if self.readbacks.is_empty() {
            return Vec::new();
        }
        let _ = gpu.device.poll(wgpu::PollType::Poll);
        let mut done = Vec::new();
        self.readbacks.retain(|r| match r.state.load(Ordering::Acquire) {
            0 => true,
            1 => {
                let bytes = {
                    let view = r.staging.slice(..).get_mapped_range();
                    match r.rows {
                        None => view.to_vec(),
                        Some((padded, row, h)) => {
                            let mut out = Vec::with_capacity((row * h) as usize);
                            for y in 0..h as usize {
                                let at = y * padded as usize;
                                out.extend_from_slice(&view[at..at + row as usize]);
                            }
                            out
                        }
                    }
                };
                r.staging.unmap();
                done.push((r.token, Ok(bytes)));
                false
            }
            _ => {
                done.push((r.token, Err("the GPU could not hand the result back".into())));
                false
            }
        });
        done
    }
}

/// FLSL's library a compute shader gets: the maths, hashes, noise and fbm.
/// What follows them is the surface pass's own (its ambient occlusion and
/// texture sampling), which needs the pass around it and has no meaning
/// here.
fn support_library() -> &'static str {
    let all = floptle_shader::stdlib::SUPPORT_WGSL;
    all.find("// AO gated").map_or(all, |i| &all[..i])
}

/// The name an entry point is compiled under, so it can never be the name of
/// a WGSL built-in (`step` is one, and the library calls it).
fn entry_name(entry: &str) -> String {
    format!("floptle_entry_{entry}")
}

/// The source with only `entry` left as an entry point, renamed by
/// [`entry_name`], and the name it was renamed to. The file's other entry
/// points are blanked out, newlines kept so a line still names the file's
/// own line. One file can then hold `seed` and `step`, each bound with only
/// what it uses: a call declares the bindings it passes, and an entry point
/// that names one this call does not pass would fail the whole file.
/// `None` when the file has no `@compute` entry point of that name.
fn entry_module(source: &str, entry: &str) -> Option<(String, String)> {
    let bytes = source.as_bytes();
    let mut keep = source.to_string().into_bytes();
    let mut found = None;
    let mut at = 0;
    while let Some(off) = source[at..].find("@compute") {
        let start = at + off;
        let fn_at = start + source[start..].find("fn ")?;
        let name_start = fn_at + 3 + source[fn_at + 3..].len() - source[fn_at + 3..].trim_start().len();
        let name_len = source[name_start..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
        let name = &source[name_start..name_start + name_len];
        let open = name_start + source[name_start..].find('{')?;
        let close = matching_brace(bytes, open)?;
        if name == entry {
            found = Some(name_start);
        } else {
            // From the first attribute of this entry point to its closing brace.
            let from = attributes_start(bytes, start);
            for b in &mut keep[from..=close] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
        at = close + 1;
    }
    let name_start = found?;
    let mut out = String::from_utf8(keep).ok()?;
    let renamed = entry_name(entry);
    out.replace_range(name_start..name_start + entry.len(), &renamed);
    Some((out, renamed))
}

/// Where the run of attributes (`@workgroup_size(8, 8) @compute`) that ends at
/// `at` begins.
fn attributes_start(b: &[u8], at: usize) -> usize {
    let mut i = at;
    loop {
        let mut j = i;
        while j > 0 && b[j - 1].is_ascii_whitespace() {
            j -= 1;
        }
        // `@name(...)`: step back over the parenthesis, then the name.
        let mut k = j;
        if k > 0 && b[k - 1] == b')' {
            let mut depth = 0i32;
            while k > 0 {
                k -= 1;
                match b[k] {
                    b')' => depth += 1,
                    b'(' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut n = k;
        while n > 0 && (b[n - 1].is_ascii_alphanumeric() || b[n - 1] == b'_') {
            n -= 1;
        }
        if n > 0 && n < k && b[n - 1] == b'@' {
            i = n - 1;
        } else {
            return i;
        }
    }
}

/// The `}` closing the `{` at `open`, past comments.
fn matching_brace(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::entry_module;

    #[test]
    fn one_entry_point_is_kept_and_the_others_blanked_line_for_line() {
        let src = "fn helper() -> f32 { return 1.0; }\n@compute @workgroup_size(8, 8)\nfn seed(@builtin(global_invocation_id) id: vec3<u32>) {\n  // a } in a comment\n  out_a[0] = 1.0;\n}\n@workgroup_size(64) @compute\nfn step(@builtin(global_invocation_id) id: vec3<u32>) {\n  if (true) { b[0] = helper(); }\n}\n";
        let (seed, name) = entry_module(src, "seed").unwrap();
        assert_eq!(name, "floptle_entry_seed");
        assert!(seed.contains("fn floptle_entry_seed(") && seed.contains("fn helper()"), "{seed}");
        assert!(!seed.contains("step") && !seed.contains("b[0]"), "{seed}");
        assert_eq!(seed.lines().count(), src.lines().count(), "every line stays where it was");
        let (step, _) = entry_module(src, "step").unwrap();
        assert!(step.contains("@workgroup_size(64) @compute\nfn floptle_entry_step(") && !step.contains("out_a"), "{step}");
        assert!(entry_module(src, "nope").is_none());
    }
}
