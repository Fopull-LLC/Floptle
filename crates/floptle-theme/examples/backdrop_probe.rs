//! Render a theme's backdrop to a PNG, to look at it.
//!
//! ```text
//! cargo run -p floptle-theme --example backdrop_probe -- <theme dir | built-in id> [out.png] [seconds] [region]
//! ```
//!
//! Draws `region`'s first shader layer (default `ground`) full-window at
//! 1280×720 and time `seconds`, then lays the region's veil over it, so the
//! picture is what a panel shows.

use floptle_theme::model::{Layer, Origin};
use floptle_theme::source::{self, Source};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let what = args.get(1).cloned().unwrap_or_else(|| "galaxy".into());
    let out = args.get(2).cloned().unwrap_or_else(|| "target/backdrop_probe.png".into());
    let time: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20.0);
    let region = args.get(4).cloned().unwrap_or_else(|| "ground".into());
    let src = if std::path::Path::new(&what).is_dir() {
        Source::Dir(what.clone().into())
    } else if what.ends_with(".floptletheme") {
        Source::Zip(what.clone().into())
    } else {
        Source::Builtin(Box::leak(what.clone().into_boxed_str()))
    };
    let theme = source::load(&src, Origin::Builtin, true).unwrap_or_else(|e| panic!("{e}"));
    let (fill, layers) = theme.surface(&region);
    let Some(Layer::Shader { shader, colors, params, scale, speed, .. }) = layers.iter().find(|l| matches!(l, Layer::Shader { .. })).cloned() else {
        panic!("{region} has no shader layer");
    };
    let body = floptle_theme::backdrop::builtin_source(&shader)
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(&theme.assets.files[shader.as_str()]).into_owned());
    floptle_theme::backdrop::validate(&body).unwrap_or_else(|e| panic!("{e}"));

    let (w, h) = (1280u32, 720u32);
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).expect("adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("device");
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(floptle_theme::backdrop::full_source(&body).into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            wgpu::BindGroupLayoutEntry { binding: 0, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None },
            wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }, count: None },
            wgpu::BindGroupLayoutEntry { binding: 2, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
        ],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
    let fmt = wgpu::TextureFormat::Rgba8Unorm;
    let pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&pl),
        vertex: wgpu::VertexState { module: &module, entry_point: Some("bd_vs"), compilation_options: Default::default(), buffers: &[] },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState { module: &module, entry_point: Some("bd_fs"), compilation_options: Default::default(), targets: &[Some(fmt.into())] }),
        multiview_mask: None,
        cache: None,
    });
    let c = |r: floptle_theme::Rgba| r.to_f32();
    let mut u: Vec<f32> = vec![w as f32, h as f32, w as f32, h as f32, time * speed, scale, 1.0, 0.0];
    for col in colors {
        u.extend(c(col));
    }
    u.extend(params);
    u.extend([0.5, 0.5, 0.0, 0.0]);
    let ubuf = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (u.len() * 4) as u64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
    queue.write_buffer(&ubuf, 0, bytemuck_cast(&u));
    // The image binding: the layer's, or white.
    let (iw, ih, ipx) = match layers.iter().find_map(|l| if let Layer::Shader { image: Some(p), .. } = l { Some(p.clone()) } else { None }) {
        Some(p) => {
            let i = image::load_from_memory(&theme.assets.files[p.as_str()]).unwrap().to_rgba8();
            (i.width(), i.height(), i.into_raw())
        }
        None => (1, 1, vec![255u8; 4]),
    };
    let itex = device.create_texture(&wgpu::TextureDescriptor { label: None, size: wgpu::Extent3d { width: iw, height: ih, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: fmt, usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST, view_formats: &[] });
    queue.write_texture(itex.as_image_copy(), &ipx, wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * iw), rows_per_image: Some(ih) }, wgpu::Extent3d { width: iw, height: ih, depth_or_array_layers: 1 });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor { address_mode_u: wgpu::AddressMode::Repeat, address_mode_v: wgpu::AddressMode::Repeat, mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, ..Default::default() });
    let iview = itex.create_view(&Default::default());
    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&iview) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
        ],
    });
    let tex = device.create_texture(&wgpu::TextureDescriptor { label: None, size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: fmt, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
    let view = tex.create_view(&Default::default());
    let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (w * h * 4) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipe);
        pass.set_bind_group(0, &bg, &[]);
        pass.draw(0..3, 0..1);
    }
    enc.copy_texture_to_buffer(tex.as_image_copy(), wgpu::TexelCopyBufferInfo { buffer: &readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) } }, wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 });
    queue.submit([enc.finish()]);
    // `FLOPTLE_BENCH=N`: draw it N more times at the default backdrop size
    // (half of 1920×1080, so a quarter of the pixels) and report the GPU
    // time per draw, waited for one draw at a time.
    if let Some(n) = std::env::var("FLOPTLE_BENCH").ok().and_then(|v| v.parse::<u32>().ok()) {
        let bview = device
            .create_texture(&wgpu::TextureDescriptor { label: None, size: wgpu::Extent3d { width: 960, height: 540, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: fmt, usage: wgpu::TextureUsages::RENDER_ATTACHMENT, view_formats: &[] })
            .create_view(&Default::default());
        let mut u2 = u.clone();
        u2[0] = 960.0;
        u2[1] = 540.0;
        queue.write_buffer(&ubuf, 0, bytemuck_cast(&u2));
        let mut times = Vec::new();
        for _ in 0..n {
            let t0 = std::time::Instant::now();
            let mut enc = device.create_command_encoder(&Default::default());
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &bview, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipe);
                pass.set_bind_group(0, &bg, &[]);
                pass.draw(0..3, 0..1);
            }
            queue.submit([enc.finish()]);
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            times.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!("{what}: median {:.3} ms per draw at 960x540 (p90 {:.3})", times[times.len() / 2], times[times.len() * 9 / 10]);
        queue.write_buffer(&ubuf, 0, bytemuck_cast(&u));
    }
    readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let data = readback.slice(..).get_mapped_range().to_vec();
    // The veil over it, as a panel would show it: the right half veiled, the
    // left half bare, so both are in one picture.
    let [fr, fg, fb, fa] = fill.0.map(|v| v as f32);
    let a = fa / 255.0;
    let mut img = image::RgbaImage::from_raw(w, h, data).unwrap();
    for (x, _, p) in img.enumerate_pixels_mut() {
        if x >= w / 2 {
            for (i, f) in [fr, fg, fb].iter().enumerate() {
                p[i] = (p[i] as f32 * (1.0 - a) + f * a).round() as u8;
            }
        }
        p[3] = 255;
    }
    img.save(&out).unwrap();
    println!("{out}");
}

fn bytemuck_cast(v: &[f32]) -> &[u8] {
    bytemuck::cast_slice(v)
}
