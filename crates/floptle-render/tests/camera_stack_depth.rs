//! **A stacked layer's depth reaches the frame's depth, where it drew.**
//!
//! Depth of field, AO and fog read the frame's depth. A stacked camera's
//! cockpit used to leave it as the base camera saw it, so a cockpit 0.6 m away
//! was blurred like the hills behind it (card 0569). A card is drawn into the
//! left half of a stack layer and the layer's depth laid into a frame depth
//! cleared to far: the left half takes the card's depth, through the matrix it
//! is given, and the right half, where the layer drew nothing, keeps far.

use floptle_render::{instance_of, plane, Globals, Gpu, Raster, StackComposite, StackLayer};
use glam::{Mat4, Vec3};

const SIZE: u32 = 32;

fn read_depth(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<f32> {
    let row = 256u32; // SIZE * 4 bytes, padded to the copy alignment
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("depth-readback"),
        size: (row * SIZE) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::DepthOnly },
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(SIZE) } },
        wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([enc.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    let data = slice.get_mapped_range();
    let mut out = Vec::with_capacity((SIZE * SIZE) as usize);
    for y in 0..SIZE {
        let start = (y * row) as usize;
        out.extend(bytemuck::cast_slice::<u8, f32>(&data[start..start + (SIZE * 4) as usize]));
    }
    out
}

/// The frame's depth after laying the layer in through `to_base`.
fn frame_depth(gpu: &Gpu, raster: &mut Raster, pass: &StackComposite, to_base: Mat4) -> Vec<f32> {
    let layer = StackLayer::new(gpu, SIZE, SIZE);
    // A card over the left half of the layer, at depth 0.5.
    let mesh = raster.register(gpu, &plane(1.0), None);
    let model = Mat4::from_translation(Vec3::new(-1.0, 0.0, 0.5));
    let globals = Globals { view_proj: Mat4::IDENTITY.to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(gpu, layer.color_view(), layer.depth_view(), globals, &[(mesh, None, instance_of(model, [1.0; 3]))], Some([0.0; 4]), None);

    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("frame-depth"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("clear"),
        color_attachments: &[],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    gpu.queue.submit([enc.finish()]);
    pass.composite_depth(gpu, &layer, &view, to_base);
    read_depth(gpu, &depth)
}

#[test]
fn a_stack_layer_s_depth_lands_where_it_drew_through_its_matrix() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so nothing can be drawn");
        return;
    }
    let mut raster = Raster::new(&gpu);
    let pass = StackComposite::new(&gpu);
    let at = |d: &[f32], x: u32, y: u32| d[(y * SIZE + x) as usize];
    let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);

    let same = frame_depth(&gpu, &mut raster, &pass, Mat4::IDENTITY);
    assert!((at(&same, 4, 16) - 0.5).abs() < 1e-3, "the layer's depth did not reach the frame: {}", at(&same, 4, 16));
    assert_eq!(at(&same, 28, 16), 1.0, "the frame's depth changed where the layer drew nothing");

    // Through a matrix that halves depth: the frame takes the base camera's
    // depth for the point, not the layer's.
    let halved = frame_depth(&gpu, &mut raster, &pass, Mat4::from_scale(Vec3::new(1.0, 1.0, 0.5)));
    assert!((at(&halved, 4, 16) - 0.25).abs() < 1e-3, "the layer's depth was not converted: {}", at(&halved, 4, 16));
    assert_eq!(at(&halved, 28, 16), 1.0);

    let refused = pollster::block_on(scope.pop()).map(|e| e.to_string());
    assert_eq!(refused, None);
}
