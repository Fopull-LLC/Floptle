//! Headless probe for the shader-graph node previews: builds the preview
//! atlas module for a couple of example shaders (fragment + sdf) exactly like
//! the editor does, renders it with stand-in bindings, and writes PNGs.
//!
//! Run: cargo run -p floptle-render --example preview_tiles_probe -- <outdir>

use floptle_render::Gpu;
use floptle_shader::graph::build_view;
use floptle_shader::ir;
use floptle_shader::preview::{preview_targets, transpile_preview, CompiledPreview};
use floptle_shader::Stage;

const TILE: u32 = 128;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let shaders = [("water", src_of("water.flsl")), ("wobbleOrb", src_of("wobbleOrb.flsl"))];
    let gpu = Gpu::headless(64, 64);
    for (name, src) in shaders {
        let compiled = build(&src);
        let out = format!("{dir}/preview_{name}.png");
        render_atlas(&gpu, &compiled, &out);
        println!("wrote {out} ({} tiles, {}x{})", compiled.tiles, compiled.cols, compiled.rows);
    }
}

fn src_of(file: &str) -> String {
    floptle_shader::examples::EXAMPLES
        .iter()
        .find(|(n, _)| *n == file)
        .map(|(_, s)| s.to_string())
        .expect("example exists")
}

fn build(src: &str) -> CompiledPreview {
    let ir = floptle_shader::parse(src).expect("parses");
    let ck = ir::check(&ir).expect("checks");
    let view = build_view(&ir, Some(&ck));
    let targets: Vec<_> = preview_targets(&ir, &view).into_iter().map(|(_, t)| t).collect();
    let compiled = transpile_preview(&ir, &ck, &targets).expect("preview transpiles");
    floptle_shader::validate_module(&compiled.wgsl)
        .unwrap_or_else(|d| panic!("naga rejected preview: {}", d.message));
    compiled
}

fn render_atlas(gpu: &Gpu, compiled: &CompiledPreview, out: &str) {
    let device = &gpu.device;
    let (w, h) = (compiled.cols * TILE, compiled.rows * TILE);
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pv"),
        source: wgpu::ShaderSource::Wgsl(compiled.wgsl.as_str().into()),
    });
    let ubo = |binding: u32| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let texture = |binding: u32| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let sampler_e = |binding: u32| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    };
    let mk = |entries: &[wgpu::BindGroupLayoutEntry]| {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries })
    };
    let layouts = match compiled.stage {
        Stage::Fragment => {
            let mut g2 = vec![ubo(0)];
            for i in 0..compiled.textures.len() as u32 {
                g2.push(texture(1 + 2 * i));
                g2.push(sampler_e(2 + 2 * i));
            }
            vec![mk(&[ubo(0), ubo(1), ubo(2)]), mk(&[texture(0), sampler_e(1)]), mk(&g2)]
        }
        Stage::Sdf => vec![mk(&[ubo(0), ubo(1)])],
    };
    let refs: Vec<Option<&wgpu::BindGroupLayout>> = layouts.iter().map(Some).collect();
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &refs,
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pv"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_pv"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_pv"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    });

    let mkbuf = |size: u64, data: &[f32]| {
        let b = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bytes: Vec<u8> = data.iter().flat_map(|x| x.to_le_bytes()).collect();
        gpu.queue.write_buffer(&b, 0, &bytes);
        b
    };
    let mut g = [0f32; 160];
    g[16..19].copy_from_slice(&[0.30, 0.45, 0.84]);
    g[20..23].copy_from_slice(&[1.0, 0.98, 0.92]);
    g[24..27].copy_from_slice(&[0.32, 0.32, 0.36]);
    let g_buf = mkbuf(640, &g);
    let mut gl = [0f32; 264];
    gl[0] = 12.5; // "time"
    if compiled.stage == Stage::Sdf {
        for (i, u) in compiled.uniforms.iter().enumerate().take(16) {
            gl[8 + i * 4..8 + i * 4 + 4].copy_from_slice(&u.default);
        }
    }
    let gl_buf = mkbuf(1056, &gl);
    let mut pv = [0f32; 260];
    pv[0] = compiled.cols as f32;
    pv[1] = compiled.rows as f32;
    pv[2] = compiled.tiles as f32;
    pv[3] = TILE as f32;
    let mut lane = 4usize;
    let irr = floptle_shader::parse(
        floptle_shader::examples::EXAMPLES
            .iter()
            .find(|(_, s)| {
                let x = floptle_shader::parse(s).unwrap();
                x.name == ir_name(compiled)
            })
            .map(|(_, s)| *s)
            .unwrap(),
    )
    .unwrap();
    for (id, lanes) in &compiled.dyn_slots {
        match &irr.expr(*id).kind {
            floptle_shader::ir::ExprKind::Num(n) if *lanes == 1 => pv[lane] = *n as f32,
            floptle_shader::ir::ExprKind::ColorLit(c) if *lanes == 4 => {
                pv[lane..lane + 4].copy_from_slice(c)
            }
            _ => {}
        }
        lane += *lanes as usize;
    }
    let pv_buf = mkbuf(1040, &pv);
    let mut p = [0f32; 128];
    for (i, u) in compiled.uniforms.iter().enumerate().take(16) {
        p[i * 4..i * 4 + 4].copy_from_slice(&u.default);
    }
    let p_buf = mkbuf(512, &p);

    // A grey checker for every texture binding.
    let n = 64u32;
    let mut px = Vec::new();
    for y in 0..n {
        for x in 0..n {
            let on = ((x / 8) + (y / 8)) % 2 == 0;
            px.extend_from_slice(if on { &[190u8, 190, 195, 255] } else { &[120, 120, 126, 255] });
        }
    }
    let checker = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &checker,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &px,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(n * 4), rows_per_image: Some(n) },
        wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
    );
    let cview = checker.create_view(&Default::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    fn bufe(binding: u32, b: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
        wgpu::BindGroupEntry { binding, resource: b.as_entire_binding() }
    }
    let mut binds = Vec::new();
    match compiled.stage {
        Stage::Fragment => {
            binds.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &pipeline.get_bind_group_layout(0),
                entries: &[bufe(0, &g_buf), bufe(1, &gl_buf), bufe(2, &pv_buf)],
            }));
            binds.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &pipeline.get_bind_group_layout(1),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&cview),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            }));
            let mut entries = vec![bufe(0, &p_buf)];
            for i in 0..compiled.textures.len() as u32 {
                entries.push(wgpu::BindGroupEntry {
                    binding: 1 + 2 * i,
                    resource: wgpu::BindingResource::TextureView(&cview),
                });
                entries.push(wgpu::BindGroupEntry {
                    binding: 2 + 2 * i,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                });
            }
            binds.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &pipeline.get_bind_group_layout(2),
                entries: &entries,
            }));
        }
        Stage::Sdf => {
            binds.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &pipeline.get_bind_group_layout(0),
                entries: &[bufe(0, &gl_buf), bufe(1, &pv_buf)],
            }));
        }
    }

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
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
        rp.set_pipeline(&pipeline);
        for (i, bg) in binds.iter().enumerate() {
            rp.set_bind_group(i as u32, bg, &[]);
        }
        rp.draw(0..3, 0..1);
    }
    gpu.queue.submit([encoder.finish()]);
    save_png(gpu, &target, w, h, out);
}

fn ir_name(c: &CompiledPreview) -> String {
    // The probe only runs the two known examples; match by texture/uniform
    // shape is overkill — recover the name from stage.
    match c.stage {
        Stage::Fragment => "water".into(),
        Stage::Sdf => "wobbleOrb".into(),
    }
}

fn save_png(gpu: &Gpu, tex: &wgpu::Texture, w: u32, h: u32, path: &str) {
    let bpp = 4u32;
    let unpadded = w * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&pixels).unwrap();
}
