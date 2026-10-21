//! wgpu viewer. The picture is letterboxed; the cursor and status line sit on top.

use deskbridge_proto::CursorShape;

use crate::decode::Picture;
use crate::error::PlatformError;

const HUD_W: u32 = 640;
const HUD_H: u32 = 32;

const SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}
struct Uniforms {
    view: vec2<f32>,
}
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> uniforms: Uniforms;

@vertex
fn vs(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var out: VsOut;
    let ndc = (pos / uniforms.view) * 2.0 - vec2<f32>(1.0, 1.0);
    out.pos = vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

#[derive(Clone, Copy, Debug)]
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
    Letterbox { x: (win_w - width) * 0.5, y: (win_h - height) * 0.5, width, height }
}

fn content_pixel(box_: Letterbox, tex_w: f32, tex_h: f32, mx: f32, my: f32) -> Option<(i32, i32)> {
    if mx < box_.x || my < box_.y || mx >= box_.x + box_.width || my >= box_.y + box_.height {
        return None;
    }
    let u = (mx - box_.x) / box_.width;
    let v = (my - box_.y) / box_.height;
    Some(((u * tex_w) as i32, (v * tex_h) as i32))
}

#[derive(Clone, Copy)]
pub struct CursorDraw {
    pub x: i32,
    pub y: i32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    sampler: wgpu::Sampler,
    bgl: wgpu::BindGroupLayout,
    pipeline: Option<wgpu::RenderPipeline>,
    format: wgpu::TextureFormat,
}

pub struct Renderer {
    instance: wgpu::Instance,
    gpu: Gpu,
    surface: Option<wgpu::Surface<'static>>,
    config: Option<wgpu::SurfaceConfiguration>,
    picture: Option<Textured>,
    cursor: Option<Textured>,
    hud: Option<Textured>,
    drawable: (f64, f64),
    frame: (u32, u32),
    cursor_w: u32,
    cursor_h: u32,
    hotspot_x: i32,
    hotspot_y: i32,
}

struct Textured {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    width: u32,
    height: u32,
}

impl Renderer {
    pub fn new() -> Result<Self, PlatformError> {
        if wgpu::Instance::enabled_backend_features().is_empty() {
            return Err(PlatformError::new("no GPU backend in this wgpu build"));
        }
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .ok_or_else(|| PlatformError::new("no GPU adapter"))?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
            .map_err(|err| PlatformError::new(format!("GPU device did not start ({err})")))?;
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("deskbridge"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let shape = arrow_shape();
        let gpu = Gpu {
            device,
            queue,
            sampler,
            bgl,
            pipeline: None,
            format: wgpu::TextureFormat::Bgra8Unorm,
        };
        let cursor = make_texture(&gpu, u32::from(shape.width), u32::from(shape.height), &swizzle_bgra(&shape.bgra))?;
        let hud = make_texture(&gpu, HUD_W, HUD_H, &vec![0; (HUD_W * HUD_H * 4) as usize])?;
        Ok(Self {
            instance,
            cursor: Some(cursor),
            hud: Some(hud),
            gpu,
            surface: None,
            config: None,
            picture: None,
            drawable: (1.0, 1.0),
            frame: (1, 1),
            cursor_w: u32::from(shape.width),
            cursor_h: u32::from(shape.height),
            hotspot_x: i32::from(shape.hotspot_x),
            hotspot_y: i32::from(shape.hotspot_y),
        })
    }

    pub fn set_cursor_bgra(&mut self, width: u16, height: u16, hotspot_x: u16, hotspot_y: u16, bgra: &[u8]) {
        let expected = usize::from(width) * usize::from(height) * 4;
        if bgra.len() != expected || width == 0 || height == 0 {
            return;
        }
        if let Ok(texture) = make_texture(&self.gpu, u32::from(width), u32::from(height), &swizzle_bgra(bgra)) {
            self.cursor = Some(texture);
            self.cursor_w = u32::from(width);
            self.cursor_h = u32::from(height);
            self.hotspot_x = i32::from(hotspot_x);
            self.hotspot_y = i32::from(hotspot_y);
        }
    }

    pub fn attach_window(&mut self, window: &winit::window::Window) {
        let scale = window.scale_factor();
        let size = window.inner_size();
        let target = unsafe { wgpu::SurfaceTargetUnsafe::from_window(window) };
        let Ok(target) = target else { return };
        let Ok(surface) = (unsafe { self.instance.create_surface_unsafe(target) }) else { return };
        let surface: wgpu::Surface<'static> = surface;
        self.surface = Some(surface);
        self.resize(f64::from(size.width), f64::from(size.height), scale);
    }

    pub fn resize(&mut self, width: f64, height: f64, _scale: f64) {
        let width = width.max(1.0) as u32;
        let height = height.max(1.0) as u32;
        self.drawable = (f64::from(width), f64::from(height));
        let Some(surface) = &self.surface else { return };
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        if self.gpu.pipeline.is_none() || self.gpu.format != format {
            self.gpu.format = format;
            self.gpu.pipeline = Some(pipeline(&self.gpu.device, &self.gpu.bgl, format));
        }
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

    pub fn present(&mut self, picture: Option<&Picture>, cursor: Option<CursorDraw>, hud: &str) -> Result<(), PlatformError> {
        if self.surface.is_none() || self.gpu.pipeline.is_none() {
            return Ok(());
        }
        if let Some(picture) = picture {
            if self.picture.as_ref().map(|tex| tex.width != picture.width || tex.height != picture.height).unwrap_or(true) {
                self.picture = Some(make_texture(&self.gpu, picture.width, picture.height, &swizzle_bgra(&picture.bgra))?);
            } else if let Some(tex) = &self.picture {
                write_rgba(&self.gpu.queue, &tex.texture, picture.width, picture.height, &swizzle_bgra(&picture.bgra));
            }
            self.frame = (picture.width, picture.height);
        }
        let hud_pixels = paint_hud(hud);
        if let Some(hud_tex) = &self.hud {
            write_rgba(&self.gpu.queue, &hud_tex.texture, HUD_W, HUD_H, &hud_pixels);
        }
        let current = {
            let Some(surface) = self.surface.as_ref() else { return Ok(()) };
            surface.get_current_texture()
        };
        let frame = match current {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                let (w, h) = self.drawable;
                self.resize(w, h, 1.0);
                return Ok(());
            }
            Err(err) => return Err(PlatformError::new(err.to_string())),
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("deskbridge"),
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
            let Some(pipeline) = self.gpu.pipeline.as_ref() else { return Ok(()) };
            pass.set_pipeline(pipeline);
            let (view_w, view_h) = self.drawable;
            if let Some(picture) = &self.picture {
                let fitted = letterbox(view_w as f32, view_h as f32, picture.width as f32, picture.height as f32);
                Self::draw_quad(&self.gpu.device, &self.gpu.queue, &mut pass, picture, fitted.x, fitted.y, fitted.width, fitted.height, view_w as f32, view_h as f32);
            }
            if let Some(cursor) = cursor {
                let (tex_w, tex_h) = self.frame;
                let fitted = letterbox(view_w as f32, view_h as f32, tex_w as f32, tex_h as f32);
                if tex_w > 1 && fitted.width > 1.0 {
                    if let Some(sprite) = &self.cursor {
                        let scale = fitted.width / tex_w as f32;
                        let x = fitted.x + (cursor.x as f32 / tex_w as f32) * fitted.width - self.hotspot_x as f32 * scale;
                        let y = fitted.y + (cursor.y as f32 / tex_h as f32) * fitted.height - self.hotspot_y as f32 * scale;
                        Self::draw_quad(&self.gpu.device, &self.gpu.queue, &mut pass, sprite, x, y, self.cursor_w as f32 * scale, self.cursor_h as f32 * scale, view_w as f32, view_h as f32);
                    }
                }
            }
            if let Some(hud_tex) = &self.hud {
                Self::draw_quad(&self.gpu.device, &self.gpu.queue, &mut pass, hud_tex, 12.0, 12.0, HUD_W as f32, HUD_H as f32, view_w as f32, view_h as f32);
            }
        }
        self.gpu.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }

    fn draw_quad<'a>(
        device: &'a wgpu::Device,
        queue: &'a wgpu::Queue,
        pass: &mut wgpu::RenderPass<'a>,
        sprite: &'a Textured,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        view_w: f32,
        view_h: f32,
    ) {
        let verts: [[f32; 4]; 6] = [
            [x, y, 0.0, 0.0],
            [x + w, y, 1.0, 0.0],
            [x, y + h, 0.0, 1.0],
            [x + w, y, 1.0, 0.0],
            [x + w, y + h, 1.0, 1.0],
            [x, y + h, 0.0, 1.0],
        ];
        let bytes = bytemuck_bytes(&verts);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buffer, 0, bytes);
        let view = [view_w, view_h, 0.0, 0.0];
        queue.write_buffer(&sprite.uniform, 0, unsafe {
            std::slice::from_raw_parts(view.as_ptr().cast::<u8>(), 16)
        });
        pass.set_bind_group(0, &sprite.bind, &[]);
        pass.set_vertex_buffer(0, buffer.slice(..));
        pass.draw(0..6, 0..1);
        // The encoder keeps the buffer id. Dropping it before submit frees the allocation.
        std::mem::forget(buffer);
    }
}

fn make_texture(gpu: &Gpu, width: u32, height: u32, rgba: &[u8]) -> Result<Textured, PlatformError> {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_rgba(&gpu.queue, &texture, width.max(1), height.max(1), rgba);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let uniform = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &gpu.bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&gpu.sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
            ],
        });
        Ok(Textured { texture, view, bind, uniform, width, height })
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // The window is dropped before the renderer. Forget the surface so wgpu does not
        // destroy a swapchain whose window is already gone.
        if let Some(surface) = self.surface.take() {
            std::mem::forget(surface);
        }
    }
}

fn pipeline(device: &wgpu::Device, bgl: &wgpu::BindGroupLayout, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("deskbridge"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[bgl],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("deskbridge"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: 16,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &[
                    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
                    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 8, shader_location: 1 },
                ],
            }],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
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
        multiview: None,
        cache: None,
    })
}

fn write_rgba(queue: &wgpu::Queue, texture: &wgpu::Texture, width: u32, height: u32, rgba: &[u8]) {
    let row = width as usize * 4;
    let padded = align256(row);
    let mut bytes = vec![0u8; padded * height as usize];
    for y in 0..height as usize {
        let src = y * row;
        let dst = y * padded;
        let n = row.min(rgba.len().saturating_sub(src));
        if n > 0 {
            bytes[dst..dst + n].copy_from_slice(&rgba[src..src + n]);
        }
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &bytes,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded as u32), rows_per_image: Some(height) },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
}

fn align256(value: usize) -> usize {
    value.div_ceil(256) * 256
}

fn swizzle_bgra(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = vec![0u8; bgra.len()];
    for chunk in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        chunk.1[0] = chunk.0[2];
        chunk.1[1] = chunk.0[1];
        chunk.1[2] = chunk.0[0];
        chunk.1[3] = chunk.0[3];
    }
    rgba
}

fn bytemuck_bytes(verts: &[[f32; 4]; 6]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(verts.as_ptr().cast::<u8>(), std::mem::size_of_val(verts)) }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let waker = futures_util::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::yield_now();
    }
}

pub(crate) fn arrow_shape() -> CursorShape {
    const SIZE: usize = 16;
    let mut bgra = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let outline = x == 0 || y == 0 || x == y || (x < 4 && y > 10) || (y < 4 && x > 10);
            let fill = x > 0 && y > 0 && x < y && x < 11 && y < 11;
            let (b, g, r, a) = if outline {
                (255, 255, 255, 255)
            } else if fill {
                (255, 255, 255, 255)
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
    CursorShape { width: SIZE as u16, height: SIZE as u16, hotspot_x: 0, hotspot_y: 0, bgra }
}

fn paint_hud(text: &str) -> Vec<u8> {
    let mut pixels = vec![0u8; (HUD_W * HUD_H * 4) as usize];
    let scale = 2usize;
    for (index, ch) in text.chars().take(36).enumerate() {
        let glyph = glyph(ch);
        let origin_x = 4 + index * 8 * scale;
        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..8 {
                if bits & (0x80 >> gx) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = origin_x + gx * scale + sx;
                        let y = 6 + gy * scale + sy;
                        if x >= HUD_W as usize || y >= HUD_H as usize {
                            continue;
                        }
                        let i = (y * HUD_W as usize + x) * 4;
                        pixels[i] = 255;
                        pixels[i + 1] = 220;
                        pixels[i + 2] = 32;
                        pixels[i + 3] = 230;
                    }
                }
            }
        }
    }
    pixels
}

fn glyph(ch: char) -> [u8; 8] {
    match ch.to_ascii_uppercase() {
        ' ' => [0; 8],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x60, 0x60],
        ':' => [0x00, 0x60, 0x60, 0x00, 0x00, 0x60, 0x60, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00, 0x00],
        '/' => [0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x00],
        '%' => [0x62, 0x64, 0x08, 0x10, 0x20, 0x4C, 0x8C, 0x00],
        '0' => [0x3C, 0x66, 0x6E, 0x76, 0x66, 0x66, 0x3C, 0x00],
        '1' => [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00],
        '2' => [0x3C, 0x66, 0x06, 0x0C, 0x18, 0x30, 0x7E, 0x00],
        '3' => [0x3C, 0x66, 0x06, 0x1C, 0x06, 0x66, 0x3C, 0x00],
        '4' => [0x0C, 0x1C, 0x3C, 0x6C, 0x7E, 0x0C, 0x0C, 0x00],
        '5' => [0x7E, 0x60, 0x7C, 0x06, 0x06, 0x66, 0x3C, 0x00],
        '6' => [0x1C, 0x30, 0x60, 0x7C, 0x66, 0x66, 0x3C, 0x00],
        '7' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x00],
        '8' => [0x3C, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x3C, 0x00],
        '9' => [0x3C, 0x66, 0x66, 0x3E, 0x06, 0x0C, 0x38, 0x00],
        'A' => [0x18, 0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x00],
        'B' => [0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x7C, 0x00],
        'C' => [0x3C, 0x66, 0x60, 0x60, 0x60, 0x66, 0x3C, 0x00],
        'D' => [0x78, 0x6C, 0x66, 0x66, 0x66, 0x6C, 0x78, 0x00],
        'E' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x7E, 0x00],
        'F' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x00],
        'G' => [0x3C, 0x66, 0x60, 0x6E, 0x66, 0x66, 0x3C, 0x00],
        'H' => [0x66, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00],
        'I' => [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00],
        'M' => [0x63, 0x77, 0x7F, 0x6B, 0x63, 0x63, 0x63, 0x00],
        'P' => [0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x00],
        'R' => [0x7C, 0x66, 0x66, 0x7C, 0x6C, 0x66, 0x66, 0x00],
        'S' => [0x3C, 0x66, 0x60, 0x3C, 0x06, 0x66, 0x3C, 0x00],
        'W' => [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00],
        _ => [0x00, 0x7E, 0x42, 0x42, 0x42, 0x42, 0x7E, 0x00],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_preserves_a_wide_picture() {
        let fitted = letterbox(1000.0, 500.0, 2000.0, 1000.0);
        assert!(fitted.x.abs() < 0.1 && fitted.y.abs() < 0.1);
        let pixel = content_pixel(fitted, 2000.0, 1000.0, 500.0, 250.0).unwrap();
        assert!((pixel.0 - 1000).abs() < 2);
    }
}
