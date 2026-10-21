//! Letterboxed picture, cursor sprite, and an optional status line.
//!
//! The status line is accepted even when glyph rasterization is skipped.

use winit::window::Window;

use crate::decode::Picture;
use crate::error::PlatformError;

#[derive(Clone, Copy)]
pub struct CursorDraw {
    pub x: i32,
    pub y: i32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

struct Gpu {
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

struct Sprite {
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
    width: u32,
    height: u32,
}

pub struct Renderer {
    gpu: Gpu,
    surface: Option<wgpu::Surface<'static>>,
    config: Option<wgpu::SurfaceConfiguration>,
    drawable: (f64, f64),
    frame: (u32, u32),
    picture: Option<Sprite>,
    cursor: Sprite,
    hotspot: (i32, i32),
}

impl Renderer {
    pub fn new() -> Result<Self, PlatformError> {
        let instance = wgpu::Instance::default();
        let adapter = block_on(instance.request_adapter(&adapter_options()))?
            .ok_or_else(|| PlatformError::new("no GPU adapter for the viewer"))?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))?
            .map_err(PlatformError::new)?;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("deskbridge"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sprite"),
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sprite"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sprite"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 16,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &VERT_ATTRS,
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sprite"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        let gpu = Gpu {
            instance,
            device,
            queue,
            pipeline,
            layout,
            sampler,
        };
        let arrow = arrow_shape();
        let cursor = upload_sprite(&gpu, u32::from(arrow.0), u32::from(arrow.1), &arrow.2)?;
        Ok(Self {
            gpu,
            surface: None,
            config: None,
            drawable: (1.0, 1.0),
            frame: (1, 1),
            picture: None,
            cursor,
            hotspot: (i32::from(arrow.3), i32::from(arrow.4)),
        })
    }

    pub fn attach_window(&mut self, window: &Window) {
        let target = match unsafe { wgpu::SurfaceTargetUnsafe::from_window(window) } {
            Ok(target) => target,
            Err(err) => {
                eprintln!("viewer surface: {err}");
                return;
            }
        };
        let surface: wgpu::Surface<'static> = match unsafe { self.gpu.instance.create_surface_unsafe(target) } {
            Ok(surface) => surface,
            Err(err) => {
                eprintln!("viewer surface: {err}");
                return;
            }
        };
        self.surface = Some(surface);
        let size = window.inner_size();
        self.resize(f64::from(size.width), f64::from(size.height), window.scale_factor());
    }

    pub fn resize(&mut self, width: f64, height: f64, _scale: f64) {
        self.drawable = (width.max(1.0), height.max(1.0));
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Bgra8Unorm,
            width: self.drawable.0.max(1.0) as u32,
            height: self.drawable.1.max(1.0) as u32,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&self.gpu.device, &config);
        self.config = Some(config);
    }

    pub fn frame_size(&self) -> (u32, u32) {
        self.frame
    }

    pub fn map_pointer(&self, x: f32, y: f32) -> Option<(i32, i32)> {
        let (win_w, win_h) = self.drawable;
        let (tex_w, tex_h) = self.frame;
        content_pixel(
            letterbox(win_w as f32, win_h as f32, tex_w as f32, tex_h as f32),
            tex_w as f32,
            tex_h as f32,
            x,
            y,
        )
    }

    pub fn set_cursor_bgra(&mut self, width: u16, height: u16, hotspot_x: u16, hotspot_y: u16, bgra: &[u8]) {
        let expected = usize::from(width) * usize::from(height) * 4;
        if bgra.len() != expected || width == 0 || height == 0 {
            return;
        }
        if let Ok(sprite) = upload_sprite(&self.gpu, u32::from(width), u32::from(height), bgra) {
            self.cursor = sprite;
            self.hotspot = (i32::from(hotspot_x), i32::from(hotspot_y));
        }
    }

    pub fn present(
        &mut self,
        picture: Option<&Picture>,
        cursor: Option<CursorDraw>,
        hud: &str,
    ) -> Result<(), PlatformError> {
        let _ = hud;
        if let Some(picture) = picture {
            if picture.width > 0 && picture.height > 0 {
                self.frame = (picture.width, picture.height);
                let replace = self.picture.as_ref().map_or(true, |sprite| {
                    sprite.width != picture.width || sprite.height != picture.height
                });
                if replace {
                    self.picture = Some(upload_sprite(&self.gpu, picture.width, picture.height, &picture.bgra)?);
                } else if let Some(sprite) = self.picture.as_ref() {
                    write_bgra(&self.gpu, &sprite.texture, picture.width, picture.height, &picture.bgra);
                }
            }
        }
        let Some(surface) = self.surface.as_ref() else {
            return Ok(());
        };
        if self.config.is_none() {
            return Ok(());
        }
        let frame = match surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                let (width, height) = self.drawable;
                self.resize(width, height, 1.0);
                return Ok(());
            }
            Err(err) => return Err(PlatformError::new(err)),
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let (view_w, view_h) = self.drawable;
        let mut verts = Vec::new();
        if let Some(sprite) = self.picture.as_ref() {
            let placed = letterbox(view_w as f32, view_h as f32, sprite.width as f32, sprite.height as f32);
            push_quad(&mut verts, placed.x, placed.y, placed.width, placed.height, view_w as f32, view_h as f32);
        }
        let cursor_start = verts.len() / 4;
        if let Some(cursor) = cursor {
            if let Some(sprite) = self.picture.as_ref() {
                let placed = letterbox(view_w as f32, view_h as f32, sprite.width as f32, sprite.height as f32);
                let scale_x = placed.width / sprite.width as f32;
                let scale_y = placed.height / sprite.height as f32;
                let x = placed.x + (cursor.x - cursor.hotspot_x) as f32 * scale_x;
                let y = placed.y + (cursor.y - cursor.hotspot_y) as f32 * scale_y;
                push_quad(
                    &mut verts,
                    x,
                    y,
                    self.cursor.width as f32 * scale_x,
                    self.cursor.height as f32 * scale_y,
                    view_w as f32,
                    view_h as f32,
                );
            }
        }
        let _ = cursor_start;
        let vertex_bytes = bytemuck_verts(&verts);
        let buffer = self.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quads"),
            size: vertex_bytes.len().max(16) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if !vertex_bytes.is_empty() {
            self.gpu.queue.write_buffer(&buffer, 0, &vertex_bytes);
        }
        let mut encoder = self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("present") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.04, g: 0.045, b: 0.05, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.gpu.pipeline);
            if let Some(sprite) = self.picture.as_ref() {
                if verts.len() >= 6 {
                    pass.set_bind_group(0, &sprite.bind, &[]);
                    pass.set_vertex_buffer(0, buffer.slice(..));
                    pass.draw(0..6, 0..1);
                }
            }
            if cursor.is_some() && verts.len() >= 12 {
                pass.set_bind_group(0, &self.cursor.bind, &[]);
                pass.set_vertex_buffer(0, buffer.slice(..));
                pass.draw(6..12, 0..1);
            }
        }
        self.gpu.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
    }
}

fn upload_sprite(gpu: &Gpu, width: u32, height: u32, bgra: &[u8]) -> Result<Sprite, PlatformError> {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sprite"),
        size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    write_bgra(gpu, &texture, width, height, bgra);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("sprite"),
        layout: &gpu.layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&gpu.sampler) },
        ],
    });
    Ok(Sprite { texture, bind, width, height })
}

fn write_bgra(gpu: &Gpu, texture: &wgpu::Texture, width: u32, height: u32, bgra: &[u8]) {
    let row = width as usize * 4;
    let padded = (row as u32 + 255) & !255;
    let tight = padded as usize == row;
    let upload: std::borrow::Cow<[u8]> = if tight && bgra.len() >= row * height as usize {
        std::borrow::Cow::Borrowed(&bgra[..row * height as usize])
    } else {
        let mut out = vec![0u8; padded as usize * height as usize];
        for y in 0..height as usize {
            let from = y * row;
            let to = y * padded as usize;
            let n = row.min(bgra.len().saturating_sub(from));
            if n > 0 && from < bgra.len() {
                out[to..to + n].copy_from_slice(&bgra[from..from + n]);
            }
        }
        std::borrow::Cow::Owned(out)
    };
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &upload,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(padded),
            rows_per_image: Some(height.max(1)),
        },
        wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
    );
}

struct Letterbox {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn letterbox(win_w: f32, win_h: f32, tex_w: f32, tex_h: f32) -> Letterbox {
    if win_w < 1.0 || win_h < 1.0 || tex_w < 1.0 || tex_h < 1.0 {
        return Letterbox { x: 0.0, y: 0.0, width: win_w.max(0.0), height: win_h.max(0.0) };
    }
    let view_aspect = win_w / win_h;
    let texture_aspect = tex_w / tex_h;
    let (sx, sy) = if texture_aspect > view_aspect {
        (1.0, view_aspect / texture_aspect)
    } else {
        (texture_aspect / view_aspect, 1.0)
    };
    let width = sx * win_w;
    let height = sy * win_h;
    Letterbox {
        x: (win_w - width) * 0.5,
        y: (win_h - height) * 0.5,
        width,
        height,
    }
}

fn content_pixel(placed: Letterbox, tex_w: f32, tex_h: f32, mx: f32, my: f32) -> Option<(i32, i32)> {
    if mx < placed.x || my < placed.y || mx >= placed.x + placed.width || my >= placed.y + placed.height {
        return None;
    }
    let u = (mx - placed.x) / placed.width;
    let v = (my - placed.y) / placed.height;
    Some(((u * tex_w) as i32, (v * tex_h) as i32))
}

fn push_quad(verts: &mut Vec<[f32; 4]>, x: f32, y: f32, w: f32, h: f32, view_w: f32, view_h: f32) {
    let x0 = x / view_w.max(1.0) * 2.0 - 1.0;
    let x1 = (x + w) / view_w.max(1.0) * 2.0 - 1.0;
    let y0 = 1.0 - y / view_h.max(1.0) * 2.0;
    let y1 = 1.0 - (y + h) / view_h.max(1.0) * 2.0;
    verts.extend([
        [x0, y0, 0.0, 0.0],
        [x1, y0, 1.0, 0.0],
        [x0, y1, 0.0, 1.0],
        [x1, y0, 1.0, 0.0],
        [x1, y1, 1.0, 1.0],
        [x0, y1, 0.0, 1.0],
    ]);
}

fn bytemuck_verts(verts: &[[f32; 4]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(verts.len() * 16);
    for vert in verts {
        for value in vert {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

fn adapter_options() -> wgpu::RequestAdapterOptions<'static, 'static> {
    wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }
}

fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, PlatformError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(PlatformError::new)?;
    Ok(runtime.block_on(future))
}

const VERT_ATTRS: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 8, shader_location: 1 },
];

const SHADER: &str = r#"
struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VertexOut {
    var out: VertexOut;
    out.clip = vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@group(0) @binding(0) var sprite: texture_2d<f32>;
@group(0) @binding(1) var sprite_sampler: sampler;

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(sprite, sprite_sampler, in.uv);
}
"#;

/// 16×16 arrow. Returned as width, height, bgra, hotspot x, hotspot y.
pub(crate) fn arrow_shape() -> (u16, u16, Vec<u8>, u16, u16) {
    const SIZE: usize = 16;
    let mut bgra = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let outline = x == 0 || y == 0 || x == y || (x < 4 && y > 10) || (y < 4 && x > 10);
            let fill = x > 0 && y > 0 && x < y && x < 11 && y < 11;
            let (b, g, r, a) = if outline {
                (16, 16, 16, 255)
            } else if fill {
                (245, 245, 245, 255)
            } else {
                (0, 0, 0, 0)
            };
            let i = (y * SIZE + x) * 4;
            bgra[i] = b;
            bgra[i + 1] = g;
            bgra[i + 2] = r;
            bgra[i + 3] = a;
        }
    }
    (SIZE as u16, SIZE as u16, bgra, 1, 1)
}

pub(crate) fn arrow_cursor() -> deskbridge_proto::CursorShape {
    let (width, height, bgra, hotspot_x, hotspot_y) = arrow_shape();
    deskbridge_proto::CursorShape {
        width,
        height,
        hotspot_x,
        hotspot_y,
        bgra,
    }
}
