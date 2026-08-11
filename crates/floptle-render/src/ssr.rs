//! The **scene colour history**: last frame's composited picture, kept with a
//! mip chain so a surface can reflect the scene and not only the sky.
//!
//! **Why a history and not this frame.** The engine shades forward: when a
//! fragment runs, the colours of the other pixels do not exist yet. Half of them
//! belong to draws that have not been issued. So there is no "current frame
//! colour" to sample, and the choice is between a deferred renderer — a G-buffer
//! written by every one of the raster pass's pipeline variants, and the specular
//! term moved out of the forward shader entirely — and reflecting the frame that
//! HAS finished. This is the second. What it costs is one frame of lag on the
//! contents of a reflection, which is invisible on anything but a mirror bolted
//! to a whip-panning camera; what it saves is the entire deferred rewrite.
//!
//! **It is captured after compositing, before post.** So it holds the scene in
//! linear HDR with the raymarched world, the raster meshes, the palette quantise
//! and the 2D light pass already in it — but no tonemap, no bloom, no grade.
//! That is the correct thing to reflect: a reflection is part of the scene and
//! must go through the tonemap WITH it, not arrive pre-tonemapped and get
//! mapped a second time.
//!
//! **The mip chain is what makes a rough reflection cheap.** Roughness picks a
//! level, exactly as it does for the sky in [`crate::env`], so a blurred
//! reflection costs the same one tap a mirror does instead of a spiral of them.
//! Both chains index by `sqrt(roughness)` so a surface that reflects some sky
//! and some scene blurs both by the same amount — otherwise the two halves of
//! one reflection would disagree, which reads as the effect being broken at
//! exactly the roughness where it should be least noticeable.
//!
//! **The history carries the camera it was taken from.** The world is
//! camera-relative (ADR-0015), so the previous frame's view-projection cannot be
//! used as it was taken — a point standing still in the world has different
//! coordinates in each frame. `prev_view_proj` folds in how far the camera moved,
//! the same correction motion blur makes; see `SceneHistory::prev_view_proj`.

use floptle_core::math::{DVec3, Mat4};

/// Last frame's composited scene, with a roughness mip chain.
pub struct SceneHistory {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    /// One view per level: level 0 is the blit target, the rest the chain.
    levels: Vec<wgpu::TextureView>,
    sampler: wgpu::Sampler,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    /// `src` bind per downsample step (level i → level i+1).
    down_binds: Vec<wgpu::BindGroup>,
    mips: u32,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    /// The camera the stored picture was taken from, and its view-projection —
    /// `None` until the first capture, which is what makes the first frame of a
    /// scene reflect the sky instead of whatever was on screen before it.
    taken: Option<(Mat4, DVec3)>,
}

/// Half resolution. A reflection is read through a BRDF lobe that is at its
/// sharpest a mirror and at its most common a blur, so the top of this chain is
/// already more detail than almost any surface asks for — and halving it turns
/// the copy and the whole chain into a quarter of the bandwidth. The visible
/// cost is confined to a roughness-0 mirror filling the screen, which is the one
/// case where the reflection is a picture of the room rather than a suggestion
/// of it.
pub const HISTORY_DIV: u32 = 2;

impl SceneHistory {
    pub fn new(device: &wgpu::Device, width: u32, height: u32, format: wgpu::TextureFormat) -> Self {
        let width = (width / HISTORY_DIV).max(1);
        let height = (height / HISTORY_DIV).max(1);
        // Down to 1×1: the roughest surface reflects one averaged colour, which
        // is the honest answer and keeps `sqrt(rough) * levels` in range.
        let mips = width.max(height).ilog2() + 1;
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scene-history"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let levels: Vec<wgpu::TextureView> = (0..mips)
            .map(|m| {
                tex.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("scene-history-level"),
                    base_mip_level: m,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        // Clamp on both axes. A screen has no wrap: a ray that leaves the frame
        // has left the data, and repeating would answer it with the opposite
        // edge of the room — a confident wrong answer, which is worse than the
        // sky fallback the miss test hands it instead.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("scene-history-samp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene-history-down"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // The same box-downsample the sky chain uses — one linear tap at the
        // destination texel centre. Shared source, separate pipeline: the sky is
        // always `Rgba16Float` and this follows the scene format, which a
        // headless probe leaves at 8-bit.
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scene-history-down"),
            source: wgpu::ShaderSource::Wgsl(crate::env::DOWN_WGSL.into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene-history-down"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scene-history-down"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let down_binds = (0..mips.saturating_sub(1))
            .map(|m| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("scene-history-down"),
                    layout: &layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&levels[m as usize]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                    ],
                })
            })
            .collect();

        Self {
            tex,
            view,
            levels,
            sampler,
            pipeline,
            layout,
            down_binds,
            mips,
            width,
            height,
            format,
            taken: None,
        }
    }

    /// The whole chain, for sampling.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    pub fn mips(&self) -> u32 {
        self.mips
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.tex
    }

    /// Does this history hold a picture yet? Until it does, shading must reflect
    /// the sky alone — see [`prev_view_proj`](Self::prev_view_proj).
    pub fn is_primed(&self) -> bool {
        self.taken.is_some()
    }

    /// Match the composited resolution. Returns whether it rebuilt, which also
    /// **drops the stored picture**: a resize changes what every texel means, and
    /// reflecting the old frame through the new projection would smear one frame
    /// of the previous window size across the scene.
    pub fn resize_to(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> bool {
        let w = (width / HISTORY_DIV).max(1);
        let h = (height / HISTORY_DIV).max(1);
        if w == self.width && h == self.height && format == self.format {
            return false;
        }
        *self = Self::new(device, width, height, format);
        true
    }

    /// Copy the composited scene into level 0 and build the chain, recording the
    /// camera it was taken from.
    ///
    /// `src` is the post chain's input view — the scene after everything that
    /// draws into it and before anything that grades it.
    pub fn capture(
        &mut self,
        gpu: &crate::Gpu,
        src: &wgpu::TextureView,
        view_proj: Mat4,
        cam_world: DVec3,
    ) {
        let device = &gpu.device;
        let mut encoder = device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("scene-history") });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene-history-copy"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src) },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene-history-copy"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.levels[0],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &bind, &[]);
            rp.draw(0..3, 0..1);
        }
        for (i, b) in self.down_binds.iter().enumerate() {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene-history-down"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.levels[i + 1],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, b, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
        self.taken = Some((view_proj, cam_world));
    }

    /// The matrix that turns a point in **this** frame's camera-relative space
    /// into the stored picture's clip space, or `None` when nothing is stored.
    ///
    /// The world is camera-relative, so the recorded view-projection is not
    /// usable as it was taken: a rock that has not moved sits at `world - cam`
    /// in each frame's coordinates and those differ by exactly how far the camera
    /// went. Pre-translating by that delta is what turns "where is this point in
    /// the old picture" into a question about the scene instead of about the
    /// origin — the same correction motion blur makes, and the same one that,
    /// left out, makes every reflection slide whenever the camera dollies.
    ///
    /// The delta is computed in `f64` and narrowed after subtracting, so a
    /// camera a million units from the origin still gets a millimetre-accurate
    /// frame-to-frame offset.
    pub fn prev_view_proj(&self, cam_world: DVec3) -> Option<Mat4> {
        let (vp, at) = self.taken?;
        Some(reproject(vp, at, cam_world))
    }

    /// A 1×1 stand-in for a renderer with no history — the same role the sky's
    /// empty map and the depth prepass's fallback play. Shading reads the
    /// DIMENSIONS to decide whether there is a scene to reflect, so there is no
    /// separate flag that could disagree with the binding.
    pub fn empty(device: &wgpu::Device) -> (wgpu::TextureView, wgpu::Sampler) {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scene-history-empty"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: crate::env::ENV_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("scene-history-empty-samp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        (tex.create_view(&wgpu::TextureViewDescriptor::default()), sampler)
    }
}

/// The reprojection itself, free of the GPU resources so it can be checked
/// without a device: `vp`/`at` are the stored picture's view-projection and
/// camera world position, `cam_world` is where the camera is now.
///
/// Split out because this is the whole of the camera-relative correction, and
/// getting it wrong does not fail loudly — it slides every reflection by a few
/// pixels whenever the camera moves, which reads as "reflections are noisy".
fn reproject(vp: Mat4, at: DVec3, cam_world: DVec3) -> Mat4 {
    vp * Mat4::from_translation((cam_world - at).as_vec3())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The correction that makes a reflection stay put when the camera moves.
    /// A point standing still in the WORLD must land on the same place in the
    /// stored picture no matter where the camera has walked to since.
    #[test]
    fn a_still_point_lands_where_it_was_however_far_the_camera_moved() {
        let proj = Mat4::perspective_rh(1.0, 1.6, 0.1, 1000.0);
        // Frame A: camera a long way from the origin (large-world), looking down
        // -Z. The view matrix carries no translation (ADR-0015), so with an
        // identity rotation the view-projection IS the projection.
        let cam_a = DVec3::new(1.0e6, 20.0, -3.0e5);
        let vp_a = proj;
        // A rock 10 units in front of the camera in frame A.
        let rock_world = cam_a + DVec3::new(0.0, 0.0, -10.0);
        let clip_a = vp_a * (rock_world - cam_a).as_vec3().extend(1.0);

        // Frame B: the camera has walked 2 units right and 1 forward.
        let cam_b = cam_a + DVec3::new(2.0, 0.0, -1.0);
        let in_b = (rock_world - cam_b).as_vec3();
        let clip_b = reproject(vp_a, cam_a, cam_b) * in_b.extend(1.0);

        let ndc = |c: floptle_core::math::Vec4| c.truncate() / c.w;
        let (a, b) = (ndc(clip_a), ndc(clip_b));
        assert!(
            (a - b).length() < 1e-4,
            "the rock moved in the stored picture: {a:?} vs {b:?} — reflections would slide",
        );

        // And the guard: WITHOUT the correction it does move, so this test is
        // measuring the fix and not an identity that would pass either way.
        let naive = ndc(vp_a * in_b.extend(1.0));
        assert!((a - naive).length() > 1e-3, "the uncorrected matrix must be visibly wrong");
    }

    /// A camera that has not moved needs no correction at all — the stored
    /// matrix stands. Guards against a delta computed with the operands swapped,
    /// which is right at zero and wrong in both directions either side of it.
    #[test]
    fn a_still_camera_reprojects_by_the_stored_matrix_exactly() {
        let vp = Mat4::perspective_rh(1.0, 1.6, 0.1, 1000.0);
        let at = DVec3::new(-4.0e5, 7.0, 12.0);
        assert_eq!(reproject(vp, at, at), vp);

        // …and the sign: a camera that moved FORWARD must find a still point
        // further away in the old picture, never nearer.
        let p_world = at + DVec3::new(0.0, 0.0, -10.0);
        let moved = at + DVec3::new(0.0, 0.0, -4.0);
        let c = reproject(vp, at, moved) * (p_world - moved).as_vec3().extend(1.0);
        assert!((c.w - 10.0).abs() < 1e-3, "clip w is the old distance: {}", c.w);
    }
}
