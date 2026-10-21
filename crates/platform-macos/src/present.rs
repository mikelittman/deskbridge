use std::ffi::c_void;

use apple_cf::iosurface::IOSurface;
use screencapturekit::metal::{
    autoreleasepool, IOSurfaceMetalExt, MTLBlendFactor, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLStoreAction, MTLVertexFormat, MTLVertexStepFunction, MetalBuffer,
    MetalCommandQueue, MetalDevice, MetalLayer, MetalLibrary, MetalRenderCommandEncoder,
    MetalRenderPassDescriptor, MetalRenderPipelineDescriptor, MetalRenderPipelineState,
    MetalVertexDescriptor, ResourceOptions, SHADER_SOURCE, Uniforms,
};

use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::cursor::arrow_shape;
use crate::decode::Picture;
use crate::error::PlatformError;

const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const HUD_W: usize = 640;
const HUD_H: usize = 32;

const OVERLAY_SHADER: &str = r"
struct OverlayIn {
    float2 pos [[attribute(0)]];
    float2 uv [[attribute(1)]];
    float4 color [[attribute(2)]];
};
struct OverlayOut {
    float4 position [[position]];
    float2 uv;
    float4 color;
};
vertex OverlayOut vertex_overlay(OverlayIn in [[stage_in]], constant Uniforms& uniforms [[buffer(1)]]) {
    OverlayOut out;
    float2 ndc = (in.pos / uniforms.viewport_size) * 2.0 - 1.0;
    ndc.y = -ndc.y;
    out.position = float4(ndc, 0.0, 1.0);
    out.uv = in.uv;
    out.color = in.color;
    return out;
}
fragment float4 fragment_overlay(OverlayOut in [[stage_in]], texture2d<float> tex [[texture(0)]]) {
    constexpr sampler s(mag_filter::nearest, min_filter::nearest);
    return tex.sample(s, in.uv) * in.color;
}
";

#[repr(C)]
#[derive(Clone, Copy)]
struct Vert {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

#[derive(Clone, Copy, Debug)]
pub struct Letterbox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub fn letterbox(win_w: f32, win_h: f32, tex_w: f32, tex_h: f32) -> Letterbox {
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

pub fn content_pixel(box_: Letterbox, tex_w: f32, tex_h: f32, mx: f32, my: f32) -> Option<(i32, i32)> {
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

pub struct Renderer {
    device: MetalDevice,
    queue: MetalCommandQueue,
    bgra: MetalRenderPipelineState,
    ycbcr: MetalRenderPipelineState,
    overlay: MetalRenderPipelineState,
    layer: MetalLayer,
    uniforms: MetalBuffer,
    cursor: IOSurface,
    cursor_w: u32,
    cursor_h: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    hud: IOSurface,
    drawable: (f64, f64),
    frame: (u32, u32),
}

impl Renderer {
    pub fn new() -> Result<Self, PlatformError> {
        let device = MetalDevice::system_default()
            .ok_or_else(|| PlatformError::new("no Metal device"))?;
        let queue = device
            .create_command_queue()
            .ok_or_else(|| PlatformError::new("no Metal command queue"))?;
        let source = format!("{SHADER_SOURCE}\n{OVERLAY_SHADER}");
        let library = device
            .create_library_with_source(&source)
            .map_err(PlatformError::new)?;
        let bgra = pipeline(&device, &library, "vertex_fullscreen", "fragment_textured", false, false)?;
        let ycbcr = pipeline(&device, &library, "vertex_fullscreen", "fragment_ycbcr", false, false)?;
        let overlay = pipeline(&device, &library, "vertex_overlay", "fragment_overlay", true, true)?;
        let uniforms = device
            .create_buffer(Uniforms::BYTE_LEN, ResourceOptions::STORAGE_MODE_SHARED)
            .ok_or_else(|| PlatformError::new("uniform buffer"))?;
        let shape = arrow_shape();
        let cursor = surface_from_bgra(shape.width as usize, shape.height as usize, &shape.bgra)?;
        let hud = IOSurface::create(HUD_W, HUD_H, BGRA, 4)
            .ok_or_else(|| PlatformError::new("HUD surface"))?;
        Ok(Self {
            device,
            queue,
            bgra,
            ycbcr,
            overlay,
            layer: MetalLayer::new(),
            uniforms,
            cursor,
            cursor_w: u32::from(shape.width),
            cursor_h: u32::from(shape.height),
            hotspot_x: i32::from(shape.hotspot_x),
            hotspot_y: i32::from(shape.hotspot_y),
            hud,
            drawable: (1.0, 1.0),
            frame: (1, 1),
        })
    }

    pub fn set_cursor_bgra(&mut self, width: u16, height: u16, hotspot_x: u16, hotspot_y: u16, bgra: &[u8]) {
        let expected = usize::from(width) * usize::from(height) * 4;
        if bgra.len() != expected {
            return;
        }
        if let Ok(surface) = surface_from_bgra(width as usize, height as usize, bgra) {
            self.cursor = surface;
            self.cursor_w = u32::from(width);
            self.cursor_h = u32::from(height);
            self.hotspot_x = i32::from(hotspot_x);
            self.hotspot_y = i32::from(hotspot_y);
        }
    }

    pub fn attach_window(&mut self, window: &winit::window::Window) {
        let scale = window.scale_factor();
        let size = window.inner_size();
        let Ok(handle) = window.window_handle() else {
            return;
        };
        if let RawWindowHandle::AppKit(appkit) = handle.as_raw() {
            self.attach(
                appkit.ns_view.as_ptr(),
                scale,
                f64::from(size.width),
                f64::from(size.height),
            );
        }
    }

    pub fn attach(&mut self, ns_view: *mut c_void, scale: f64, width: f64, height: f64) {
        self.layer.set_device(&self.device);
        self.layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        self.layer.set_presents_with_transaction(false);
        self.resize(width, height, scale);
        unsafe {
            let wants = sel_registerName(c"setWantsLayer:".as_ptr());
            let set_layer = sel_registerName(c"setLayer:".as_ptr());
            msg_send_bool(ns_view, wants, true);
            msg_send_ptr(ns_view, set_layer, self.layer.as_ptr());
        }
    }

    pub fn resize(&mut self, width: f64, height: f64, scale: f64) {
        let width = width.max(1.0);
        let height = height.max(1.0);
        self.drawable = (width, height);
        self.layer.set_drawable_size(width, height);
        unsafe {
            let set_scale = sel_registerName(c"setContentsScale:".as_ptr());
            msg_send_f64(self.layer.as_ptr(), set_scale, scale);
        }
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

    pub fn present(
        &mut self,
        picture: Option<&Picture>,
        cursor: Option<CursorDraw>,
        hud: &str,
    ) -> Result<(), PlatformError> {
        autoreleasepool(|| self.present_in_pool(picture, cursor, hud))
    }

    fn present_in_pool(
        &mut self,
        picture: Option<&Picture>,
        cursor: Option<CursorDraw>,
        hud: &str,
    ) -> Result<(), PlatformError> {
        let Some(drawable) = self.layer.next_drawable() else {
            return Ok(());
        };
        let (view_w, view_h) = self.drawable;
        paint_hud(&self.hud, hud);
        let pass = MetalRenderPassDescriptor::new();
        pass.set_color_attachment_texture(0, &drawable.texture())
            .map_err(PlatformError::new)?;
        pass.set_color_attachment_load_action(0, MTLLoadAction::Clear)
            .map_err(PlatformError::new)?;
        pass.set_color_attachment_store_action(0, MTLStoreAction::Store)
            .map_err(PlatformError::new)?;
        pass.set_color_attachment_clear_color(0, 0.04, 0.045, 0.05, 1.0)
            .map_err(PlatformError::new)?;
        let command = self
            .queue
            .command_buffer()
            .ok_or_else(|| PlatformError::new("command buffer"))?;
        let encoder = command
            .render_command_encoder(&pass)
            .map_err(PlatformError::new)?;
        if let Some(picture) = picture {
            self.draw_picture(&encoder, picture.surface(), view_w as f32, view_h as f32)?;
        }
        self.draw_sprites(&encoder, view_w as f32, view_h as f32, cursor)?;
        encoder.end_encoding().map_err(PlatformError::new)?;
        command.present_drawable(&drawable).map_err(PlatformError::new)?;
        command.commit().map_err(PlatformError::new)?;
        Ok(())
    }

    fn draw_picture(
        &mut self,
        encoder: &MetalRenderCommandEncoder,
        surface: &IOSurface,
        view_w: f32,
        view_h: f32,
    ) -> Result<(), PlatformError> {
        let Some(textures) = surface.create_metal_textures(&self.device) else {
            return Ok(());
        };
        self.frame = (textures.width as u32, textures.height as u32);
        let pipeline = if textures.is_ycbcr() { &self.ycbcr } else { &self.bgra };
        encoder
            .set_render_pipeline_state(pipeline)
            .map_err(PlatformError::new)?;
        write_uniforms(
            &self.uniforms,
            view_w,
            view_h,
            textures.width as f32,
            textures.height as f32,
            surface.pixel_format(),
        );
        encoder
            .set_vertex_buffer(&self.uniforms, 0, 0)
            .map_err(PlatformError::new)?;
        encoder
            .set_fragment_buffer(&self.uniforms, 0, 0)
            .map_err(PlatformError::new)?;
        encoder
            .set_fragment_texture(&textures.plane0, 0)
            .map_err(PlatformError::new)?;
        if let Some(plane1) = &textures.plane1 {
            encoder
                .set_fragment_texture(plane1, 1)
                .map_err(PlatformError::new)?;
        }
        encoder
            .draw_primitives(MTLPrimitiveType::TriangleStrip, 0, 4)
            .map_err(PlatformError::new)?;
        Ok(())
    }

    fn draw_sprites(
        &self,
        encoder: &MetalRenderCommandEncoder,
        view_w: f32,
        view_h: f32,
        cursor: Option<CursorDraw>,
    ) -> Result<(), PlatformError> {
        let mut verts = Vec::new();
        let (tex_w, tex_h) = self.frame;
        let fitted = letterbox(view_w, view_h, tex_w as f32, tex_h as f32);
        if let Some(cursor) = cursor {
            if tex_w > 1 && fitted.width > 1.0 {
                let scale = fitted.width / tex_w as f32;
                let x = fitted.x + (cursor.x as f32 / tex_w as f32) * fitted.width
                    - cursor.hotspot_x as f32 * scale;
                let y = fitted.y + (cursor.y as f32 / tex_h as f32) * fitted.height
                    - cursor.hotspot_y as f32 * scale;
                push_quad(
                    &mut verts,
                    x,
                    y,
                    self.cursor_w as f32 * scale,
                    self.cursor_h as f32 * scale,
                );
            }
        }
        push_quad(&mut verts, 12.0, 12.0, HUD_W as f32, HUD_H as f32);
        if verts.is_empty() {
            return Ok(());
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(verts.as_ptr().cast::<u8>(), std::mem::size_of_val(verts.as_slice()))
        };
        let buffer = self
            .device
            .create_buffer_with_bytes(bytes)
            .ok_or_else(|| PlatformError::new("overlay buffer"))?;
        write_uniforms(&self.uniforms, view_w, view_h, tex_w as f32, tex_h as f32, BGRA);
        encoder
            .set_render_pipeline_state(&self.overlay)
            .map_err(PlatformError::new)?;
        encoder
            .set_vertex_buffer(&buffer, 0, 0)
            .map_err(PlatformError::new)?;
        encoder
            .set_vertex_buffer(&self.uniforms, 0, 1)
            .map_err(PlatformError::new)?;
        let cursor_quads = usize::from(cursor.is_some() && tex_w > 1 && fitted.width > 1.0);
        if cursor_quads == 1 {
            if let Some(textures) = self.cursor.create_metal_textures(&self.device) {
                encoder
                    .set_fragment_texture(&textures.plane0, 0)
                    .map_err(PlatformError::new)?;
                encoder
                    .draw_primitives(MTLPrimitiveType::Triangle, 0, 6)
                    .map_err(PlatformError::new)?;
            }
        }
        if let Some(textures) = self.hud.create_metal_textures(&self.device) {
            encoder
                .set_fragment_texture(&textures.plane0, 0)
                .map_err(PlatformError::new)?;
            let start = cursor_quads * 6;
            encoder
                .draw_primitives(MTLPrimitiveType::Triangle, start, 6)
                .map_err(PlatformError::new)?;
        }
        Ok(())
    }
}

fn pipeline(
    device: &MetalDevice,
    library: &MetalLibrary,
    vertex: &str,
    fragment: &str,
    blended: bool,
    vertices: bool,
) -> Result<MetalRenderPipelineState, PlatformError> {
    let vertex_fn = library
        .get_function(vertex)
        .ok_or_else(|| PlatformError::new(format!("missing shader {vertex}")))?;
    let fragment_fn = library
        .get_function(fragment)
        .ok_or_else(|| PlatformError::new(format!("missing shader {fragment}")))?;
    let desc = MetalRenderPipelineDescriptor::new();
    desc.set_vertex_function(&vertex_fn);
    desc.set_fragment_function(&fragment_fn);
    desc.set_color_attachment_pixel_format(0, MTLPixelFormat::BGRA8Unorm)
        .map_err(PlatformError::new)?;
    if blended {
        desc.set_blending_enabled(0, true).map_err(PlatformError::new)?;
        desc.set_blend_factors(
            0,
            MTLBlendFactor::SourceAlpha,
            MTLBlendFactor::OneMinusSourceAlpha,
            MTLBlendFactor::One,
            MTLBlendFactor::OneMinusSourceAlpha,
        )
        .map_err(PlatformError::new)?;
    }
    if vertices {
        let layout = MetalVertexDescriptor::new();
        layout
            .set_attribute(0, MTLVertexFormat::Float2, 0, 0)
            .map_err(PlatformError::new)?;
        layout
            .set_attribute(1, MTLVertexFormat::Float2, 8, 0)
            .map_err(PlatformError::new)?;
        layout
            .set_attribute(2, MTLVertexFormat::Float4, 16, 0)
            .map_err(PlatformError::new)?;
        layout
            .set_layout(0, std::mem::size_of::<Vert>(), MTLVertexStepFunction::PerVertex)
            .map_err(PlatformError::new)?;
        desc.set_vertex_descriptor(&layout);
    }
    device
        .create_render_pipeline_state(&desc)
        .ok_or_else(|| PlatformError::new(format!("pipeline {vertex}/{fragment}")))
}

fn write_uniforms(buffer: &MetalBuffer, view_w: f32, view_h: f32, tex_w: f32, tex_h: f32, format: u32) {
    let bytes = Uniforms::new(view_w, view_h, tex_w, tex_h)
        .with_pixel_format(format)
        .to_bytes();
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.contents().cast(), bytes.len());
    }
    buffer.did_modify_range(0..bytes.len());
}

fn push_quad(verts: &mut Vec<Vert>, x: f32, y: f32, w: f32, h: f32) {
    let color = [1.0, 1.0, 1.0, 1.0];
    let v = |px: f32, py: f32, u: f32, tv: f32| Vert { pos: [px, py], uv: [u, tv], color };
    verts.extend([
        v(x, y, 0.0, 0.0),
        v(x + w, y, 1.0, 0.0),
        v(x, y + h, 0.0, 1.0),
        v(x + w, y, 1.0, 0.0),
        v(x + w, y + h, 1.0, 1.0),
        v(x, y + h, 0.0, 1.0),
    ]);
}

fn surface_from_bgra(width: usize, height: usize, pixels: &[u8]) -> Result<IOSurface, PlatformError> {
    let surface = IOSurface::create(width, height, BGRA, 4)
        .ok_or_else(|| PlatformError::new("cursor surface"))?;
    {
        let mut guard = surface.lock_read_write().map_err(PlatformError::new)?;
        let dst = guard
            .base_address_mut()
            .ok_or_else(|| PlatformError::new("cursor surface is not writable"))?;
        let row = guard.bytes_per_row();
        for y in 0..height {
            let src = &pixels[y * width * 4..(y + 1) * width * 4];
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(y * row), src.len());
            }
        }
    }
    Ok(surface)
}

fn paint_hud(surface: &IOSurface, text: &str) {
    let Ok(mut guard) = surface.lock_read_write() else {
        return;
    };
    let Some(dst) = guard.base_address_mut() else {
        return;
    };
    let row = guard.bytes_per_row();
    let height = guard.height();
    unsafe {
        std::ptr::write_bytes(dst, 0, row * height);
    }
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
                        if x >= guard.width() || y >= height {
                            continue;
                        }
                        unsafe {
                            let pixel = dst.add(y * row + x * 4);
                            *pixel = 32;
                            *pixel.add(1) = 220;
                            *pixel.add(2) = 255;
                            *pixel.add(3) = 230;
                        }
                    }
                }
            }
        }
    }
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
        'J' => [0x06, 0x06, 0x06, 0x06, 0x06, 0x66, 0x3C, 0x00],
        'K' => [0x66, 0x6C, 0x78, 0x70, 0x78, 0x6C, 0x66, 0x00],
        'L' => [0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00],
        'M' => [0x63, 0x77, 0x7F, 0x6B, 0x63, 0x63, 0x63, 0x00],
        'N' => [0x66, 0x76, 0x7E, 0x7E, 0x6E, 0x66, 0x66, 0x00],
        'O' => [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
        'P' => [0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x00],
        'Q' => [0x3C, 0x66, 0x66, 0x66, 0x6A, 0x6C, 0x36, 0x00],
        'R' => [0x7C, 0x66, 0x66, 0x7C, 0x6C, 0x66, 0x66, 0x00],
        'S' => [0x3C, 0x66, 0x60, 0x3C, 0x06, 0x66, 0x3C, 0x00],
        'T' => [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00],
        'U' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
        'V' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00],
        'W' => [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00],
        'X' => [0x66, 0x66, 0x3C, 0x18, 0x3C, 0x66, 0x66, 0x00],
        'Y' => [0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x18, 0x00],
        'Z' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x7E, 0x00],
        _ => [0x00, 0x7E, 0x42, 0x42, 0x42, 0x42, 0x7E, 0x00],
    }
}

#[link(name = "objc", kind = "dylib")]
#[allow(clashing_extern_declarations)]
extern "C" {
    fn sel_registerName(name: *const i8) -> *const c_void;
    #[link_name = "objc_msgSend"]
    fn msg_send_bool(obj: *mut c_void, sel: *const c_void, arg: bool);
    #[link_name = "objc_msgSend"]
    fn msg_send_ptr(obj: *mut c_void, sel: *const c_void, arg: *mut c_void);
    #[link_name = "objc_msgSend"]
    fn msg_send_f64(obj: *mut c_void, sel: *const c_void, arg: f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_preserves_a_wide_picture() {
        let fitted = letterbox(1000.0, 500.0, 2000.0, 1000.0);
        assert!((fitted.x).abs() < 0.1);
        assert!((fitted.y).abs() < 0.1);
        let pixel = content_pixel(fitted, 2000.0, 1000.0, 500.0, 250.0).unwrap();
        assert!((pixel.0 - 1000).abs() < 2);
        assert!((pixel.1 - 500).abs() < 2);
    }

    #[test]
    fn pointer_in_the_bars_is_ignored() {
        let fitted = letterbox(1000.0, 1000.0, 2000.0, 1000.0);
        assert!(fitted.y > 100.0);
        assert!(content_pixel(fitted, 2000.0, 1000.0, 500.0, 10.0).is_none());
    }
}
