//! The wgpu presentation layer (M10): one pipeline, one atlas texture, one
//! draw call. Everything on screen — glyphs, cell backgrounds, overlays,
//! cursor, tab bar — is a quad; solid quads sample the atlas's white texel.
//! Draw order in the vertex buffer is the painter's order the CPU renderer
//! already used, so translucent overlays blend identically.

use crate::render::{Atlas, ATLAS_SIZE};
use std::collections::HashMap;
use std::sync::Arc;

use winit::window::Window;

/// One vertex: pixel-space position, atlas uv, atlas page (layer), and
/// straight-alpha color.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub layer: f32,
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    screen: [f32; 2],
    _pad: [f32; 2],
}

const SHADER: &str = r#"
struct Globals { screen: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_tex: texture_2d_array<f32>;
@group(0) @binding(2) var atlas_smp: sampler;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) layer: f32,
    @location(3) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: i32,
    @location(2) color: vec4<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
    var out: VsOut;
    let ndc = vec2<f32>(
        in.pos.x / globals.screen.x * 2.0 - 1.0,
        1.0 - in.pos.y / globals.screen.y * 2.0,
    );
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = in.uv;
    out.layer = i32(in.layer + 0.5);
    out.color = in.color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return in.color * textureSample(atlas_tex, atlas_smp, in.uv, in.layer);
}
"#;

pub struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    globals_buf: wgpu::Buffer,
    atlas_tex: wgpu::Texture,
    layers: u32,
    /// Inline-image textures, keyed by the store's cookie. Dropped when the
    /// frame's panes no longer hold that image; one scrolled out of view
    /// arrives with no verts and keeps its texture.
    images: HashMap<u64, CachedImage>,
}

struct CachedImage {
    _texture: wgpu::Texture,
    bind: wgpu::BindGroup,
}

/// One image draw. The vertices are already clipped to the pane.
pub struct ImageQuad<'a> {
    pub cookie: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
    pub verts: &'a [Vertex],
}

impl Gpu {
    pub fn new(window: Arc<Window>, width: u32, height: u32) -> Result<Self, String> {
        // DX12 first (native on Windows, fastest to initialize), GL as the
        // fallback when DX12 cannot make a device. Vulkan is not a backend:
        // that feature is off, so vulkan-1.dll is never opened.
        // wgpu 30 dropped Default for InstanceDescriptor (a display handle is
        // now an explicit choice); DX12/GL ignore it, so "without" is ours.
        //
        // `Dx12Compiler::Auto` (the descriptor default) loads the bare name
        // `dxcompiler.dll`, and the application directory is searched first.
        // FXC uses `d3dcompiler_47.dll`, which `dll_search::harden_dll_search`
        // has already mapped from System32. A missing dxcompiler.dll is the
        // normal case and still renders. `WGPU_DX12_COMPILER` is not applied:
        // `dxc` would put the bare name back.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12 | wgpu::Backends::GL,
            backend_options: wgpu::BackendOptions {
                dx12: wgpu::Dx12BackendOptions {
                    shader_compiler: match crate::dll_search::dx12_shader_compiler() {
                        crate::dll_search::Dx12ShaderCompiler::Fxc => wgpu::Dx12Compiler::Fxc,
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("create_surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            // browser-style fingerprinting defence; a native terminal wants
            // the adapter's real limits, not a bucketed approximation
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("no graphics adapter: {e}"))?;
        // wgpu 30 dropped the trace-path argument
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("bayan"),
            ..Default::default()
        }))
        .map_err(|e| format!("request_device: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        // prefer a non-sRGB surface so our u8 colors hit the framebuffer
        // unshifted (identical to the CPU renderer's look)
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            // wgpu 30 made the color space explicit. Its Default variant is
            // documented as reproducing wgpu's historical behavior (Srgb for
            // our non-sRGB-format case), which is what M10 verified as
            // pixel-identical to the CPU renderer. Anything else changes the look.
            color_space: wgpu::SurfaceColorSpace::default(),
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let layers = 1u32;
        let atlas_tex = Self::make_atlas(&device, layers);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = Self::bind_group(&device, &bgl, &globals_buf, &atlas_tex, &sampler);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bayan quad shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bgl)],
            // wgpu 30 replaced push_constant_ranges with immediate data;
            // the quad pipeline uses neither
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bayan quads"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32, 3 => Float32x4
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group,
            bgl,
            sampler,
            globals_buf,
            atlas_tex,
            layers,
            images: HashMap::new(),
        })
    }

    fn make_atlas(device: &wgpu::Device, layers: u32) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    fn bind_group(
        device: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        globals: &wgpu::Buffer,
        atlas: &wgpu::Texture,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        let view = atlas.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 || (width, height) == (self.config.width, self.config.height) {
            return false;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        true
    }

    /// Sync changed CPU atlas pages; a clean page is copied only when growth
    /// recreated the texture array. Return whether sampled pixels changed,
    /// so identical quad geometry cannot hide an atlas update. 2048² RGBA
    /// has a bytes_per_row already 256-aligned.
    pub fn upload_atlas(&mut self, atlas: &mut Atlas) -> bool {
        let n = atlas.pages.len() as u32;
        let recreated = n != self.layers;
        if recreated {
            self.atlas_tex = Self::make_atlas(&self.device, n);
            self.layers = n;
            self.bind_group = Self::bind_group(
                &self.device,
                &self.bgl,
                &self.globals_buf,
                &self.atlas_tex,
                &self.sampler,
            );
        }
        let mut changed = false;
        for (layer, page) in atlas.uploads(recreated) {
            changed = true;
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas_tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: layer,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                page,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(ATLAS_SIZE * 4),
                    rows_per_image: Some(ATLAS_SIZE),
                },
                wgpu::Extent3d {
                    width: ATLAS_SIZE,
                    height: ATLAS_SIZE,
                    depth_or_array_layers: 1,
                },
            );
        }
        atlas.uploaded();
        changed
    }

    /// Upload any image the cache does not have yet, and drop the rest.
    /// A bad buffer (wrong length, zero size) is skipped rather than
    /// submitted: a corrupt placement must not fail the frame.
    fn sync_images(&mut self, images: &[ImageQuad<'_>]) {
        let live: Vec<u64> = images.iter().map(|img| img.cookie).collect();
        self.images.retain(|cookie, _| live.contains(cookie));
        for img in images {
            if img.verts.is_empty()
                || img.width == 0
                || img.height == 0
                || self.images.contains_key(&img.cookie)
            {
                continue;
            }
            let pixels = (img.width as usize)
                .saturating_mul(img.height as usize)
                .saturating_mul(4);
            if img.rgba.len() != pixels {
                continue;
            }
            let Some(cached) = self.upload_image(img) else {
                continue;
            };
            self.images.insert(img.cookie, cached);
        }
    }

    fn upload_image(&self, img: &ImageQuad<'_>) -> Option<CachedImage> {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("inline image"),
            size: wgpu::Extent3d {
                width: img.width,
                height: img.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // write_texture rows must be 256-aligned. Padding is not sampled:
        // the quad's uvs stay inside the real width.
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let tight = img.width.saturating_mul(4);
        let bpr = tight.div_ceil(align) * align;
        let padded;
        let bytes: &[u8] = if bpr == tight {
            img.rgba
        } else {
            padded = pad_rows(img.rgba, img.width, img.height, bpr);
            &padded
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bpr),
                rows_per_image: Some(img.height),
            },
            wgpu::Extent3d {
                width: img.width,
                height: img.height,
                depth_or_array_layers: 1,
            },
        );
        let bind = Self::bind_group(
            &self.device,
            &self.bgl,
            &self.globals_buf,
            &texture,
            &self.sampler,
        );
        Some(CachedImage {
            _texture: texture,
            bind,
        })
    }

    /// Render one frame of quads (painter's order). Images are drawn first,
    /// with the same pipeline, so cell text and the cursor stay on top.
    /// BG is the clear color.
    pub fn render(&mut self, verts: &[Vertex], images: &[ImageQuad<'_>], bg: (u8, u8, u8)) -> bool {
        use wgpu::util::DeviceExt;
        // wgpu 30 replaced Result<SurfaceTexture, SurfaceError> with an enum.
        // Suboptimal still carries a usable texture — render it rather than
        // dropping the frame, or a resize would blink.
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            Cst::Outdated | Cst::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    Cst::Success(f) | Cst::Suboptimal(f) => f,
                    _ => return false,
                }
            }
            // Timeout, Occluded (window hidden), Validation: skip this frame.
            _ => return false,
        };
        let view = frame.texture.create_view(&Default::default());
        let globals = Globals {
            screen: [self.config.width as f32, self.config.height as f32],
            _pad: [0.0; 2],
        };
        self.queue
            .write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
        self.sync_images(images);
        let image_bufs: Vec<(u64, wgpu::Buffer, u32)> = images
            .iter()
            .filter(|img| !img.verts.is_empty() && self.images.contains_key(&img.cookie))
            .map(|img| {
                let buf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("image quad"),
                        contents: bytemuck::cast_slice(img.verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                (img.cookie, buf, img.verts.len() as u32)
            })
            .collect();
        let vbuf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("frame quads"),
                contents: bytemuck::cast_slice(verts),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("bayan frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    // 2D surface texture: no 3D/array slice to select
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg.0 as f64 / 255.0,
                            g: bg.1 as f64 / 255.0,
                            b: bg.2 as f64 / 255.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                // single-view rendering: view 0 only
                multiview_mask: None,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            for (cookie, buf, n) in &image_bufs {
                if let Some(cached) = self.images.get(cookie) {
                    pass.set_bind_group(0, &cached.bind, &[]);
                    pass.set_vertex_buffer(0, buf.slice(..));
                    pass.draw(0..*n, 0..1);
                }
            }
            if !verts.is_empty() {
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, vbuf.slice(..));
                pass.draw(0..verts.len() as u32, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        // wgpu 30 moved present() from SurfaceTexture onto Queue
        self.queue.present(frame);
        true
    }
}

fn pad_rows(rgba: &[u8], width: u32, height: u32, bpr: u32) -> Vec<u8> {
    let tight = width as usize * 4;
    let mut buf = vec![0u8; bpr as usize * height as usize];
    for row in 0..height as usize {
        let src = row * tight;
        let dst = row * bpr as usize;
        buf[dst..dst + tight].copy_from_slice(&rgba[src..src + tight]);
    }
    buf
}
