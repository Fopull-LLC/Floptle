//! Baking the light probe volume — the editor's half of global illumination.
//!
//! The whole design in one line: **a probe is a camera**. Each one renders the
//! scene six times, once per cube face, through the same `render_world_into`
//! the Game view uses, and the six little pictures are integrated into
//! spherical harmonics by [`floptle_gi`].
//!
//! That is the decision worth defending. The obvious alternative is to trace
//! rays against the SDF field, which the engine already marches for shadows and
//! AO — but the field holds only what has been voxelised into it, and it holds
//! distance, not *colour*. Bouncing grey light off a red wall is most of the way
//! to no bounce at all. Rendering the scene means every single thing that can be
//! seen contributes what it actually looks like: meshes with their textures,
//! terrain with its splats, tilemaps, sculpted matter, custom `.flsl` materials,
//! emissive surfaces, and the sky. And it means there is no second gather to
//! keep in step with the first — a lesson this codebase has learned the hard way
//! (see `render_world_into`'s own notes).
//!
//! The cost is that a bake is thousands of real frames, so it runs a slice at a
//! time across the editor's own frames: the window stays live, the progress bar
//! moves, and Cancel works.

use floptle_core::math::DVec3;
use floptle_gi::{BakedGi, FACES, FaceStats, Probe, ProbeGrid};
use floptle_render::{Projection, RenderCamera};

/// Probes rendered between GPU syncs. Each probe is six render passes into six
/// array layers; the readback that follows is a full stall, so doing several
/// probes per stall matters more than the copies do.
const BATCH: usize = 8;

/// How long one editor frame may spend baking. The bake is deliberately not
/// allowed to own the frame: a progress bar that cannot repaint is a hang with
/// extra steps.
const FRAME_BUDGET_MS: u128 = 24;

/// A bake in progress.
pub(crate) struct GiBake {
    pub(crate) grid: ProbeGrid,
    probes: Vec<Probe>,
    stats: Vec<FaceStats>,
    /// Next probe to render, within the current bounce.
    next: usize,
    /// Which bounce is being gathered (1-based), and how many were asked for.
    pub(crate) bounce: u32,
    pub(crate) bounces: u32,
    face: u32,
    cull_mask: u32,
    /// Six-layers-per-probe scratch targets, plus the buffer they read back
    /// through. Owned by the bake so cancelling frees them.
    color: wgpu::Texture,
    depth: wgpu::Texture,
    color_buf: wgpu::Buffer,
    depth_buf: wgpu::Buffer,
    color_row: u32,
    depth_row: u32,
    format: wgpu::TextureFormat,
    started: std::time::Instant,
}

impl GiBake {
    /// Fraction done across all bounces, 0…1.
    pub(crate) fn progress(&self) -> f32 {
        let total = (self.grid.count() * self.bounces as usize).max(1) as f32;
        let done = ((self.bounce.saturating_sub(1)) as usize * self.grid.count() + self.next) as f32;
        (done / total).clamp(0.0, 1.0)
    }

    pub(crate) fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }
}

/// Everything the Inspector needs to say about GI, gathered once per frame.
///
/// A snapshot rather than a borrow because the Inspector already holds the world
/// mutably while it draws the node's own knobs, and "how big will this bake be"
/// is a question about the whole editor, not about the node.
#[derive(Clone, Copy, Default)]
pub(crate) struct GiStatus {
    pub baking: bool,
    pub progress: f32,
    pub bounce: u32,
    pub bounces: u32,
    pub seconds: f32,
    /// Probes in the bake currently on disk (0 = none yet), and its bounces.
    pub baked_probes: usize,
    pub baked_bounces: u32,
    /// The grid the node's CURRENT settings would produce. Shown before you
    /// bake, because probe count is the one number that decides whether this
    /// takes four seconds or four minutes, and it is derived from two other
    /// numbers in a way nobody should have to do in their head.
    pub planned: [u32; 3],
    /// The existing bake no longer matches those settings.
    pub stale: bool,
    pub show_only: bool,
    pub show_probes: bool,
}

impl GiStatus {
    pub(crate) fn planned_count(&self) -> usize {
        self.planned[0] as usize * self.planned[1] as usize * self.planned[2] as usize
    }
}

/// What a probe-texture upload depends on. Compared each frame; different means
/// re-upload.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct GiKey {
    center: [f32; 3],
    intensity: f32,
    leak: f32,
    normal_bias: f32,
    show_only: bool,
    generation: usize,
}

/// The probe camera's near and far planes.
///
/// Near is small so a probe close to a wall still sees it rather than clipping
/// through into the room beyond — which would be a leak baked into the data
/// itself, where no amount of sampling care can undo it. Far is modest because
/// depth precision is what the clearance measurement is made of, and nothing
/// past a kilometre changes a bounce.
const NEAR: f32 = 0.05;
const FAR: f32 = 1000.0;

/// Round a row length up to wgpu's copy alignment.
fn padded_row(bytes: u32) -> u32 {
    let a = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    bytes.div_ceil(a) * a
}

/// One IEEE half decoded to `f32`.
///
/// The scene renders into `Rgba16Float` (that is the whole point of the HDR
/// chain), and reading a bake back means decoding halves by hand — there is no
/// half-float crate in this workspace, and this is eleven lines.
fn half_to_f32(bits: u16) -> f32 {
    let sign = ((bits >> 15) & 1) as u32;
    let exp = ((bits >> 10) & 0x1f) as u32;
    let man = (bits & 0x3ff) as u32;
    let out = match exp {
        // Zero and subnormals: scale the mantissa into a normal f32 directly.
        0 => return f32::from_bits(sign << 31) + (man as f32) * 5.960_464_5e-8 * if sign == 1 { -1.0 } else { 1.0 },
        // Inf / NaN.
        0x1f => (sign << 31) | 0x7f80_0000 | (man << 13),
        _ => (sign << 31) | ((exp + 112) << 23) | (man << 13),
    };
    f32::from_bits(out)
}

/// The scene's light probe volume, if it has one: `(entity, settings)`.
///
/// The FIRST one, deliberately. Several volumes is a real thing to want later (a
/// level of rooms at different densities), but "the second one is silently
/// ignored" is a much better failure than "two volumes fight over the same
/// uniform slots and the light flickers".
///
/// A free function rather than a method because the editor's frame already holds
/// half of `self` mutably by the time the Inspector needs this (`floptle/0110`).
pub(crate) fn gi_node(
    world: &floptle_core::World,
) -> Option<(floptle_core::Entity, floptle_core::Matter)> {
    world
        .query::<floptle_core::Matter>()
        .find(|(e, m)| {
            matches!(m, floptle_core::Matter::LightProbes { .. })
                && !floptle_core::is_disabled(world, *e)
        })
        .map(|(e, m)| (e, m.clone()))
}

/// The volume's world centre and half-extent, with the node's transform applied
/// — so moving or scaling the node moves and scales the box.
pub(crate) fn gi_bounds(
    world: &floptle_core::World,
    e: floptle_core::Entity,
    half: [f32; 3],
) -> (DVec3, [f32; 3]) {
    let t = floptle_core::world_transform(world, e);
    let s = t.scale;
    (t.translation, [half[0] * s.x.abs(), half[1] * s.y.abs(), half[2] * s.z.abs()])
}

/// This frame's GI summary for the Inspector.
pub(crate) fn gi_status(
    world: &floptle_core::World,
    bake: Option<&GiBake>,
    baked: Option<&BakedGi>,
    show_only: bool,
    show_probes: bool,
) -> GiStatus {
    let mut s = GiStatus { show_only, show_probes, ..Default::default() };
    if let Some(b) = bake {
        s.baking = true;
        s.progress = b.progress();
        s.bounce = b.bounce;
        s.bounces = b.bounces;
        s.seconds = b.elapsed().as_secs_f32();
    }
    if let Some(b) = baked {
        s.baked_probes = b.probes.len();
        s.baked_bounces = b.bounces;
    }
    if let Some((e, floptle_core::Matter::LightProbes { half_extents, spacing, .. })) =
        gi_node(world)
    {
        let (center, half) = gi_bounds(world, e, half_extents);
        let grid =
            ProbeGrid::from_spacing([center.x as f32, center.y as f32, center.z as f32], half, spacing);
        s.planned = grid.dims;
        // Stale means "the data no longer describes this box": a different
        // lattice, or a box moved or resized since. It is a note, not an error —
        // the old bake keeps lighting the scene, which is much better than going
        // dark while you nudge a volume.
        s.stale = baked.is_some_and(|b| {
            b.grid.dims != grid.dims
                || b.grid.half_extent.iter().zip(grid.half_extent).any(|(a, c)| (a - c).abs() > 1e-3)
        });
    }
    s
}

impl crate::Editor {
    pub(crate) fn gi_node(&self) -> Option<(floptle_core::Entity, floptle_core::Matter)> {
        gi_node(&self.world)
    }

    pub(crate) fn gi_bounds(&self, e: floptle_core::Entity, half: [f32; 3]) -> (DVec3, [f32; 3]) {
        gi_bounds(&self.world, e, half)
    }

    /// Where this scene's bake is saved. Keyed off the scene's real relative
    /// path, not its stem: two scenes called `main.ron` in different folders are
    /// two scenes, and keying on the stem is how the terrain store once had them
    /// overwrite each other (`floptle/0111`).
    pub(crate) fn gi_path(&self) -> std::path::PathBuf {
        let mut p = self.scene_path();
        p.set_extension("fgi");
        p
    }

    /// Load the scene's bake from disk, if there is one. Called after a scene
    /// loads; a missing or stale file simply means "no GI yet".
    pub(crate) fn load_gi(&mut self) {
        self.gi_baked = std::fs::read(self.gi_path()).ok().and_then(|b| BakedGi::from_bytes(&b));
        self.gi_dirty = true;
        // A new scene is a new room. Reflection captures are not stored, so
        // there is nothing to load — but the ones in hand belong to the scene
        // that just closed, and the entities they were keyed to are gone.
        self.probe_slots.clear();
        self.recapture_reflection_probes();
    }

    /// Push the current bake + the node's knobs at the renderer.
    ///
    /// Driven by a KEY comparison rather than a dirty flag. The volume's
    /// settings can change from the Inspector, from a script, from an undo, from
    /// a prefab paste and from the node being dragged, and "remember to set the
    /// flag" is a rule five call sites have to keep — this way the only thing
    /// that has to be true is that the numbers are the numbers.
    ///
    /// What it does is an UPLOAD of a few hundred kilobytes, so intensity, leak
    /// and the debug view are all immediate; the expensive half — the probes —
    /// is untouched.
    pub(crate) fn refresh_gi(&mut self) {
        let key = self.gi_key();
        if !std::mem::take(&mut self.gi_dirty) && key == self.gi_uploaded {
            return;
        }
        let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) else {
            return;
        };
        let volume = match (&self.gi_baked, &key) {
            (Some(baked), Some(k)) if !baked.is_empty() => floptle_render::GiVolume::upload(
                gpu,
                baked,
                k.center,
                k.leak,
                k.intensity,
                k.show_only,
                k.normal_bias,
            ),
            _ => floptle_render::GiVolume::empty(gpu),
        };
        raymarch.set_gi(gpu, volume);
        self.gi_uploaded = key;
    }

    /// Everything an upload depends on. `None` = nothing to upload (no volume,
    /// or one switched off).
    fn gi_key(&self) -> Option<GiKey> {
        let (e, m) = gi_node(&self.world)?;
        let floptle_core::Matter::LightProbes {
            enabled: true, intensity, leak, normal_bias, ..
        } = m
        else {
            return None;
        };
        let c = floptle_core::world_transform(&self.world, e).translation;
        Some(GiKey {
            center: [c.x as f32, c.y as f32, c.z as f32],
            intensity,
            leak,
            normal_bias,
            // Never while baking: the probe cubes render through this same path,
            // and a bake gathered with every direct light switched off would be
            // a bake of nothing.
            show_only: self.gi_show_only && self.gi_bake.is_none(),
            // So a finished bake, a cleared one, or a second bounce all count as
            // a change without having to say so.
            generation: self.gi_baked.as_ref().map_or(0, |b| b.probes.len() * 8 + b.bounces as usize),
        })
    }

    /// Begin a bake. Returns false when there is nothing to bake.
    pub(crate) fn start_gi_bake(&mut self) -> bool {
        let Some((e, floptle_core::Matter::LightProbes {
            half_extents, spacing, bounces, quality, exclude_layers, ..
        })) = self.gi_node() else {
            return false;
        };
        let Some(gpu) = self.gpu.as_ref() else { return false };
        let (center, half) = self.gi_bounds(e, half_extents);
        let grid = ProbeGrid::from_spacing(
            [center.x as f32, center.y as f32, center.z as f32],
            half,
            spacing,
        );
        let face = quality.clamp(4, 64).next_power_of_two();
        let cull_mask = if exclude_layers.is_empty() {
            u32::MAX
        } else {
            !self.project.build_layers().mask_of(exclude_layers.iter().map(String::as_str))
        };

        let layers = (BATCH * 6) as u32;
        let format = gpu.scene_format();
        let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gi-bake-color"),
            size: wgpu::Extent3d { width: face, height: face, depth_or_array_layers: layers },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gi-bake-depth"),
            size: wgpu::Extent3d { width: face, height: face, depth_or_array_layers: layers },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: floptle_render::Gpu::DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let bpp = format.block_copy_size(None).unwrap_or(8);
        let color_row = padded_row(face * bpp);
        let depth_row = padded_row(face * 4);
        let mk = |label, row: u32| {
            gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (row * face * layers) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        };
        let n = grid.count();
        self.gi_bake = Some(GiBake {
            grid,
            probes: vec![Probe::default(); n],
            stats: vec![FaceStats::default(); n],
            next: 0,
            bounce: 1,
            bounces: bounces.clamp(1, 4),
            face,
            cull_mask,
            color,
            depth,
            color_buf: mk("gi-bake-color-read", color_row),
            depth_buf: mk("gi-bake-depth-read", depth_row),
            color_row,
            depth_row,
            format,
            started: std::time::Instant::now(),
        });
        // Bounce 1 is direct light only: the volume must not gather the light it
        // is in the middle of computing.
        self.gi_baked = None;
        self.gi_dirty = true;
        self.refresh_gi();
        true
    }

    /// `--bake-gi`: start the bake once the scene is up, then quit when it is
    /// done. Called every frame; does nothing at all in the ordinary editor.
    ///
    /// Waits for the GPU to exist rather than firing at startup, because the
    /// bake renders through the whole scene path and there is nothing to render
    /// through before the first frame has been set up.
    pub(crate) fn drive_auto_bake(&mut self) {
        let Some(started) = self.auto_bake_gi else { return };
        if !started {
            if self.gpu.is_none() {
                return;
            }
            if self.start_gi_bake() {
                self.auto_bake_gi = Some(true);
            } else {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!(
                        "--bake-gi: {} has no enabled Light Probes node, nothing to bake",
                        self.scene_rel_or_default()
                    ),
                    None,
                );
                self.pending_exit = true;
            }
            return;
        }
        if self.gi_bake.is_none() {
            self.pending_exit = true;
        }
    }

    pub(crate) fn cancel_gi_bake(&mut self) {
        if self.gi_bake.take().is_some() {
            // Put back whatever was on disk, so cancelling restores the scene's
            // lighting instead of leaving it dark.
            self.load_gi();
            self.refresh_gi();
        }
    }

    /// Advance a bake by up to one frame's worth of work. Call once per frame.
    pub(crate) fn step_gi_bake(&mut self) {
        let Some(mut bake) = self.gi_bake.take() else { return };
        let frame_start = std::time::Instant::now();

        while bake.next < bake.grid.count() {
            let batch = BATCH.min(bake.grid.count() - bake.next);
            self.render_gi_batch(&mut bake, batch);
            bake.next += batch;
            if frame_start.elapsed().as_millis() >= FRAME_BUDGET_MS {
                break;
            }
        }

        if bake.next < bake.grid.count() {
            self.gi_bake = Some(bake);
            return;
        }

        // A bounce is done: fold the clearance measurements in and publish it,
        // so the NEXT bounce's renders see this one's light. That is all
        // "multi-bounce" is — the same bake, run again, with the answer from
        // last time turned on.
        for (p, s) in bake.probes.iter_mut().zip(bake.stats.iter()) {
            s.finish(p);
        }
        let done = BakedGi {
            grid: bake.grid,
            probes: std::mem::take(&mut bake.probes),
            bounces: bake.bounce,
        };
        self.gi_baked = Some(done);
        self.gi_dirty = true;
        self.refresh_gi();

        if bake.bounce >= bake.bounces {
            let path = self.gi_path();
            let bytes = self.gi_baked.as_ref().map(|b| b.to_bytes()).unwrap_or_default();
            let msg = match std::fs::write(&path, &bytes) {
                Ok(()) => format!(
                    "baked GI: {} probes × {} bounce{} in {:.1}s → {} ({} KB)",
                    bake.grid.count(),
                    bake.bounces,
                    if bake.bounces == 1 { "" } else { "s" },
                    bake.elapsed().as_secs_f32(),
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    bytes.len() / 1024,
                ),
                Err(e) => format!("baked GI, but could not write {}: {e}", path.display()),
            };
            self.console.push(floptle_script::LogLevel::Debug, msg, None);
            return;
        }

        bake.bounce += 1;
        bake.next = 0;
        bake.probes = vec![Probe::default(); bake.grid.count()];
        bake.stats = vec![FaceStats::default(); bake.grid.count()];
        self.gi_bake = Some(bake);
    }

    /// Render `count` probes' cubes and integrate them.
    fn render_gi_batch(&mut self, bake: &mut GiBake, count: usize) {
        let face = bake.face;
        let color_views: Vec<wgpu::TextureView> = (0..count as u32 * 6)
            .map(|l| {
                bake.color.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: l,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let depth_views: Vec<wgpu::TextureView> = (0..count as u32 * 6)
            .map(|l| {
                bake.depth.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: l,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();

        for i in 0..count {
            let p = bake.grid.probe_world(bake.next + i);
            let pos = DVec3::new(p.x as f64, p.y as f64, p.z as f64);
            for (f, face_def) in FACES.iter().enumerate() {
                let cam = RenderCamera::new(
                    pos,
                    face_def.rotation(),
                    // A cube face IS a 90° square frustum. Anything else and the
                    // texel directions the integrator assumes stop matching the
                    // pixels it is handed.
                    Projection::Perspective {
                        fov_y: std::f32::consts::FRAC_PI_2,
                        near: NEAR,
                        far: FAR,
                    },
                );
                let l = i * 6 + f;
                self.render_world_into(
                    &color_views[l],
                    &depth_views[l],
                    &cam,
                    1.0,
                    0.0,
                    bake.cull_mask,
                    None,
                    (face, face),
                    // A bake is not a view anybody looks at: no prepass and no
                    // reflection history. Its whole job is to sample radiance.
                    Default::default(),
                );
            }
        }

        // One stall for the whole batch. The GPU borrow is taken HERE rather
        // than at the top, because `render_world_into` above needs `&mut self`.
        let Some(gpu) = self.gpu.as_ref() else { return };
        let layers = count as u32 * 6;
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gi-readback") });
        for (tex, buf, row) in [
            (&bake.color, &bake.color_buf, bake.color_row),
            (&bake.depth, &bake.depth_buf, bake.depth_row),
        ] {
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: buf,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(face),
                    },
                },
                wgpu::Extent3d { width: face, height: face, depth_or_array_layers: layers },
            );
        }
        gpu.queue.submit(Some(enc.finish()));
        let cslice = bake.color_buf.slice(..);
        let dslice = bake.depth_buf.slice(..);
        cslice.map_async(wgpu::MapMode::Read, |_| {});
        dslice.map_async(wgpu::MapMode::Read, |_| {});
        if gpu.device.poll(wgpu::PollType::wait_indefinitely()).is_err() {
            return;
        }
        let cmap = cslice.get_mapped_range();
        let dmap = dslice.get_mapped_range();

        let n = face as usize;
        let mut rgb = vec![[0.0f32; 3]; n * n];
        let mut dist = vec![0.0f32; n * n];
        for i in 0..count {
            let idx = bake.next + i;
            for (f, face_def) in FACES.iter().enumerate() {
                let layer = i * 6 + f;
                let cbase = layer * (bake.color_row as usize * n);
                let dbase = layer * (bake.depth_row as usize * n);
                for y in 0..n {
                    for x in 0..n {
                        let ci = cbase + y * bake.color_row as usize;
                        rgb[y * n + x] = read_rgb(&cmap[ci..], x, bake.format);
                        let di = dbase + y * bake.depth_row as usize + x * 4;
                        let d = f32::from_le_bytes(dmap[di..di + 4].try_into().unwrap_or([0; 4]));
                        let (u, v) = floptle_gi::texel_uv(x as u32, y as u32, face);
                        dist[y * n + x] = floptle_gi::radial_distance(d, NEAR, FAR, u, v);
                    }
                }
                floptle_gi::accumulate_face(
                    &mut bake.probes[idx],
                    face_def,
                    face,
                    &rgb,
                    &dist,
                    &mut bake.stats[idx],
                );
            }
        }
        drop(cmap);
        drop(dmap);
        bake.color_buf.unmap();
        bake.depth_buf.unmap();
    }
}

/// Decode texel `x` of a row, in whatever format the scene renders into.
///
/// The editor is always `Rgba16Float`; a headless GPU built without the HDR
/// flag is 8-bit, which is exactly the configuration the probe test runs under
/// on some machines. Handling both here means the bake is not quietly wrong on
/// one of them.
fn read_rgb(row: &[u8], x: usize, format: wgpu::TextureFormat) -> [f32; 3] {
    match format {
        wgpu::TextureFormat::Rgba16Float => {
            let o = x * 8;
            let h = |k: usize| {
                half_to_f32(u16::from_le_bytes(row.get(o + k..o + k + 2).map_or([0; 2], |s| {
                    s.try_into().unwrap_or([0; 2])
                })))
            };
            [h(0), h(2), h(4)]
        }
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => {
            let o = x * 4;
            let g = |k: usize| row.get(o + k).map_or(0.0, |&b| srgb_to_linear(b));
            [g(2), g(1), g(0)]
        }
        _ => {
            let o = x * 4;
            let g = |k: usize| row.get(o + k).map_or(0.0, |&b| srgb_to_linear(b));
            [g(0), g(1), g(2)]
        }
    }
}

/// An 8-bit sRGB byte back to linear light. Only the 8-bit fallback path needs
/// it; the HDR target is already linear, which is the entire reason the bake can
/// be trusted at all — integrating display-space pixels bakes the tonemap into
/// the bounce, and the result is washed out in a way no knob undoes.
fn srgb_to_linear(b: u8) -> f32 {
    let c = b as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The half decoder, against values whose bit patterns are known. A wrong
    /// exponent bias here would scale every bake by a power of two, which reads
    /// as "the GI intensity default is bad" rather than as a decoding bug.
    #[test]
    fn halves_decode_to_the_values_they_encode() {
        for (bits, want) in [
            (0x0000u16, 0.0f32),
            (0x3c00, 1.0),
            (0xbc00, -1.0),
            (0x4000, 2.0),
            (0x3800, 0.5),
            (0x3555, 0.333_251_95),
            (0x7bff, 65504.0),
        ] {
            let got = half_to_f32(bits);
            assert!((got - want).abs() <= want.abs() * 1e-6 + 1e-7, "{bits:#06x}: {got} != {want}");
        }
    }

    /// Row padding has to be the wgpu alignment, or a readback reads the next
    /// row's pixels as this row's and the bake comes out sheared.
    #[test]
    fn rows_pad_to_the_copy_alignment() {
        let a = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        for face in [4u32, 8, 16, 32, 64] {
            for bpp in [4u32, 8] {
                let r = padded_row(face * bpp);
                assert_eq!(r % a, 0, "{face}×{bpp} → {r}");
                assert!(r >= face * bpp);
            }
        }
    }
}
