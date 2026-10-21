//! VA-API probe and the shared display used by the encoder and decoder.
//!
//! libva is loaded at runtime so a machine without a GPU still builds and
//! returns an empty codec list instead of panicking.

use std::ffi::c_void;
use std::fs::File;
use std::os::fd::AsRawFd;

use deskbridge_proto::CodecId;
use libloading::{Library, Symbol};

use crate::error::PlatformError;

const PROFILE_H264_MAIN: i32 = 6;
const PROFILE_H264_HIGH: i32 = 7;
const PROFILE_HEVC_MAIN: i32 = 17;
const ENTRY_VLD: i32 = 1;
const ENTRY_ENC_SLICE: i32 = 6;

pub const VA_STATUS_SUCCESS: i32 = 0;
pub const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
pub const VA_PROGRESSIVE: i32 = 1;
pub const VA_INVALID_ID: u32 = 0xffff_ffff;
pub const VA_RC_CBR: u32 = 0x0000_0002;
pub const VA_RC_VBR: u32 = 0x0000_0004;
pub const VA_RC_CQP: u32 = 0x0000_0010;
pub const VA_FOURCC_NV12: u32 = 0x3231_564e;
pub const VA_FOURCC_BGRA: u32 = 0x4152_4742;
pub const VA_LSB_FIRST: u32 = 1;

pub const BUF_PICTURE: i32 = 0;
pub const BUF_IQ_MATRIX: i32 = 1;
pub const BUF_SLICE_PARAM: i32 = 4;
pub const BUF_SLICE_DATA: i32 = 5;
pub const BUF_ENC_CODED: i32 = 21;
pub const BUF_ENC_SEQUENCE: i32 = 22;
pub const BUF_ENC_PICTURE: i32 = 23;
pub const BUF_ENC_SLICE: i32 = 24;
pub const BUF_ENC_MISC: i32 = 27;

pub const MISC_FRAME_RATE: u32 = 0;
pub const MISC_RATE_CONTROL: u32 = 1;

pub const PICTURE_H264_INVALID: u32 = 0x0000_0001;
pub const PICTURE_H264_SHORT_TERM: u32 = 0x0000_0008;
pub const PICTURE_HEVC_INVALID: u32 = 0x0000_0001;

pub const CONFIG_RT_FORMAT: i32 = 0;
pub const CONFIG_RATE_CONTROL: i32 = 5;

type VaStatusFn = unsafe extern "C" fn(display: *mut c_void) -> i32;
type VaInitialize = unsafe extern "C" fn(display: *mut c_void, major: *mut i32, minor: *mut i32) -> i32;
type VaGetDisplayDrm = unsafe extern "C" fn(fd: i32) -> *mut c_void;
type VaQueryProfiles = unsafe extern "C" fn(display: *mut c_void, list: *mut i32, num: *mut i32) -> i32;
type VaQueryEntrypoints =
    unsafe extern "C" fn(display: *mut c_void, profile: i32, list: *mut i32, num: *mut i32) -> i32;
type VaGetConfigAttributes = unsafe extern "C" fn(
    display: *mut c_void,
    profile: i32,
    entrypoint: i32,
    attribs: *mut ConfigAttrib,
    num: i32,
) -> i32;
type VaCreateConfig = unsafe extern "C" fn(
    display: *mut c_void,
    profile: i32,
    entrypoint: i32,
    attribs: *mut ConfigAttrib,
    num: i32,
    config: *mut u32,
) -> i32;
type VaDestroyConfig = unsafe extern "C" fn(display: *mut c_void, config: u32) -> i32;
type VaCreateSurfaces = unsafe extern "C" fn(
    display: *mut c_void,
    format: u32,
    width: u32,
    height: u32,
    surfaces: *mut u32,
    num: u32,
    attribs: *mut c_void,
    num_attribs: u32,
) -> i32;
type VaDestroySurfaces = unsafe extern "C" fn(display: *mut c_void, surfaces: *mut u32, num: i32) -> i32;
type VaCreateContext = unsafe extern "C" fn(
    display: *mut c_void,
    config: u32,
    width: i32,
    height: i32,
    flag: i32,
    targets: *mut u32,
    num_targets: i32,
    context: *mut u32,
) -> i32;
type VaDestroyContext = unsafe extern "C" fn(display: *mut c_void, context: u32) -> i32;
type VaCreateBuffer = unsafe extern "C" fn(
    display: *mut c_void,
    context: u32,
    kind: i32,
    size: u32,
    num: u32,
    data: *mut c_void,
    buf: *mut u32,
) -> i32;
type VaDestroyBuffer = unsafe extern "C" fn(display: *mut c_void, buf: u32) -> i32;
type VaMapBuffer = unsafe extern "C" fn(display: *mut c_void, buf: u32, out: *mut *mut c_void) -> i32;
type VaUnmapBuffer = unsafe extern "C" fn(display: *mut c_void, buf: u32) -> i32;
type VaBeginPicture = unsafe extern "C" fn(display: *mut c_void, context: u32, target: u32) -> i32;
type VaRenderPicture = unsafe extern "C" fn(display: *mut c_void, context: u32, buffers: *mut u32, num: i32) -> i32;
type VaEndPicture = unsafe extern "C" fn(display: *mut c_void, context: u32) -> i32;
type VaSyncSurface = unsafe extern "C" fn(display: *mut c_void, surface: u32) -> i32;
type VaCreateImage =
    unsafe extern "C" fn(display: *mut c_void, format: *mut ImageFormat, width: i32, height: i32, image: *mut Image) -> i32;
type VaDestroyImage = unsafe extern "C" fn(display: *mut c_void, image: u32) -> i32;
type VaPutImage = unsafe extern "C" fn(
    display: *mut c_void,
    surface: u32,
    image: u32,
    src_x: i32,
    src_y: i32,
    src_width: u32,
    src_height: u32,
    dest_x: i32,
    dest_y: i32,
    dest_width: u32,
    dest_height: u32,
) -> i32;
type VaGetImage = unsafe extern "C" fn(
    display: *mut c_void,
    surface: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    image: u32,
) -> i32;
type VaQueryImageFormats = unsafe extern "C" fn(display: *mut c_void, list: *mut ImageFormat, num: *mut i32) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConfigAttrib {
    pub kind: i32,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Image {
    pub image_id: u32,
    pub format: ImageFormat,
    pub buf: u32,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CodedSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut c_void,
    pub next: *mut CodedSegment,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PictureH264 {
    pub picture_id: u32,
    pub frame_idx: u32,
    pub flags: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PictureHevc {
    pub picture_id: u32,
    pub pic_order_cnt: i32,
    pub flags: u32,
    pub va_reserved: [u32; 4],
}

pub struct VaApi {
    _libva: Library,
    _libdrm: Library,
    _node: File,
    display: *mut c_void,
    initialize_done: bool,
    pub(crate) terminate: VaStatusFn,
    pub(crate) get_config_attributes: VaGetConfigAttributes,
    pub(crate) create_config: VaCreateConfig,
    pub(crate) destroy_config: VaDestroyConfig,
    pub(crate) create_surfaces: VaCreateSurfaces,
    pub(crate) destroy_surfaces: VaDestroySurfaces,
    pub(crate) create_context: VaCreateContext,
    pub(crate) destroy_context: VaDestroyContext,
    pub(crate) create_buffer: VaCreateBuffer,
    pub(crate) destroy_buffer: VaDestroyBuffer,
    pub(crate) map_buffer: VaMapBuffer,
    pub(crate) unmap_buffer: VaUnmapBuffer,
    pub(crate) begin_picture: VaBeginPicture,
    pub(crate) render_picture: VaRenderPicture,
    pub(crate) end_picture: VaEndPicture,
    pub(crate) sync_surface: VaSyncSurface,
    pub(crate) create_image: VaCreateImage,
    pub(crate) destroy_image: VaDestroyImage,
    pub(crate) put_image: VaPutImage,
    pub(crate) get_image: VaGetImage,
    pub(crate) query_image_formats: VaQueryImageFormats,
    query_profiles: VaQueryProfiles,
    query_entrypoints: VaQueryEntrypoints,
    max_profiles: VaStatusFn,
    max_entrypoints: VaStatusFn,
}

unsafe impl Send for VaApi {}

impl Drop for VaApi {
    fn drop(&mut self) {
        if self.initialize_done && !self.display.is_null() {
            unsafe { (self.terminate)(self.display) };
        }
    }
}

impl VaApi {
    pub fn open() -> Result<Self, PlatformError> {
        let libva = unsafe { Library::new("libva.so.2") }
            .map_err(|_| PlatformError::new("libva.so.2 is not installed"))?;
        let libdrm = unsafe { Library::new("libva-drm.so.2") }
            .map_err(|_| PlatformError::new("libva-drm.so.2 is not installed"))?;
        let get_display_sym: Symbol<VaGetDisplayDrm> = unsafe { libdrm.get(b"vaGetDisplayDRM\0") }
            .map_err(|_| PlatformError::new("vaGetDisplayDRM is missing"))?;
        let get_display = *get_display_sym;
        drop(get_display_sym);
        let mut last = PlatformError::new("no VA-API render node");
        for path in ["/dev/dri/renderD128", "/dev/dri/renderD129"] {
            let node = match File::options().read(true).write(true).open(path) {
                Ok(file) => file,
                Err(err) => {
                    last = PlatformError::new(format!("{path}: {err}"));
                    continue;
                }
            };
            let display = unsafe { get_display(node.as_raw_fd()) };
            if display.is_null() {
                last = PlatformError::new(format!("{path} did not open a VA display"));
                continue;
            }
            let api = load_api(libva, libdrm, node, display)?;
            return Ok(api);
        }
        Err(last)
    }

    pub fn display(&self) -> *mut c_void {
        self.display
    }

    pub fn profiles(&self) -> Result<Vec<i32>, PlatformError> {
        let max = unsafe { (self.max_profiles)(self.display) }.max(1) as usize;
        let mut list = vec![0i32; max];
        let mut num = 0i32;
        self.check(
            unsafe { (self.query_profiles)(self.display, list.as_mut_ptr(), &mut num) },
            "vaQueryConfigProfiles",
        )?;
        let num = num.max(0) as usize;
        list.truncate(num.min(list.len()));
        Ok(list)
    }

    pub fn entrypoints(&self, profile: i32) -> Result<Vec<i32>, PlatformError> {
        let max = unsafe { (self.max_entrypoints)(self.display) }.max(1) as usize;
        let mut list = vec![0i32; max];
        let mut num = 0i32;
        self.check(
            unsafe { (self.query_entrypoints)(self.display, profile, list.as_mut_ptr(), &mut num) },
            "vaQueryConfigEntrypoints",
        )?;
        let num = num.max(0) as usize;
        list.truncate(num.min(list.len()));
        Ok(list)
    }

    pub fn supports(&self, profile: i32, entry: i32) -> bool {
        self.entrypoints(profile).map(|list| list.contains(&entry)).unwrap_or(false)
    }

    pub fn rate_control(&self, profile: i32, entry: i32) -> u32 {
        let mut attrib = ConfigAttrib { kind: CONFIG_RATE_CONTROL, value: 0 };
        let status = unsafe { (self.get_config_attributes)(self.display, profile, entry, &mut attrib, 1) };
        if status != VA_STATUS_SUCCESS {
            return VA_RC_CQP;
        }
        let value = attrib.value;
        if value & VA_RC_CBR != 0 {
            VA_RC_CBR
        } else if value & VA_RC_VBR != 0 {
            VA_RC_VBR
        } else if value & VA_RC_CQP != 0 {
            VA_RC_CQP
        } else {
            VA_RC_CQP
        }
    }

    pub fn check(&self, status: i32, what: &str) -> Result<(), PlatformError> {
        if status == VA_STATUS_SUCCESS {
            Ok(())
        } else {
            Err(PlatformError::new(format!("{what} failed ({status})")))
        }
    }
}

fn load_api(libva: Library, libdrm: Library, node: File, display: *mut c_void) -> Result<VaApi, PlatformError> {
    unsafe fn sym<T>(lib: &Library, name: &[u8]) -> Result<T, PlatformError>
    where
        T: Copy,
    {
        let symbol: Symbol<T> = unsafe { lib.get(name) }
            .map_err(|_| PlatformError::new(format!("missing {}", String::from_utf8_lossy(name).trim_end_matches('\0'))))?;
        Ok(*symbol)
    }
    let initialize: VaInitialize = unsafe { sym(&libva, b"vaInitialize\0") }?;
    let mut major = 0i32;
    let mut minor = 0i32;
    let status = unsafe { initialize(display, &mut major, &mut minor) };
    if status != VA_STATUS_SUCCESS {
        return Err(PlatformError::new(format!("vaInitialize failed ({status})")));
    }
    Ok(VaApi {
        terminate: unsafe { sym(&libva, b"vaTerminate\0") }?,
        get_config_attributes: unsafe { sym(&libva, b"vaGetConfigAttributes\0") }?,
        create_config: unsafe { sym(&libva, b"vaCreateConfig\0") }?,
        destroy_config: unsafe { sym(&libva, b"vaDestroyConfig\0") }?,
        create_surfaces: unsafe { sym(&libva, b"vaCreateSurfaces\0") }?,
        destroy_surfaces: unsafe { sym(&libva, b"vaDestroySurfaces\0") }?,
        create_context: unsafe { sym(&libva, b"vaCreateContext\0") }?,
        destroy_context: unsafe { sym(&libva, b"vaDestroyContext\0") }?,
        create_buffer: unsafe { sym(&libva, b"vaCreateBuffer\0") }?,
        destroy_buffer: unsafe { sym(&libva, b"vaDestroyBuffer\0") }?,
        map_buffer: unsafe { sym(&libva, b"vaMapBuffer\0") }?,
        unmap_buffer: unsafe { sym(&libva, b"vaUnmapBuffer\0") }?,
        begin_picture: unsafe { sym(&libva, b"vaBeginPicture\0") }?,
        render_picture: unsafe { sym(&libva, b"vaRenderPicture\0") }?,
        end_picture: unsafe { sym(&libva, b"vaEndPicture\0") }?,
        sync_surface: unsafe { sym(&libva, b"vaSyncSurface\0") }?,
        create_image: unsafe { sym(&libva, b"vaCreateImage\0") }?,
        destroy_image: unsafe { sym(&libva, b"vaDestroyImage\0") }?,
        put_image: unsafe { sym(&libva, b"vaPutImage\0") }?,
        get_image: unsafe { sym(&libva, b"vaGetImage\0") }?,
        query_image_formats: unsafe { sym(&libva, b"vaQueryImageFormats\0") }?,
        query_profiles: unsafe { sym(&libva, b"vaQueryConfigProfiles\0") }?,
        query_entrypoints: unsafe { sym(&libva, b"vaQueryConfigEntrypoints\0") }?,
        max_profiles: unsafe { sym(&libva, b"vaMaxNumProfiles\0") }?,
        max_entrypoints: unsafe { sym(&libva, b"vaMaxNumEntrypoints\0") }?,
        _libva: libva,
        _libdrm: libdrm,
        _node: node,
        display,
        initialize_done: true,
    })
}

fn codecs_for(entry: i32) -> Vec<CodecId> {
    let Ok(api) = VaApi::open() else {
        return Vec::new();
    };
    let Ok(profiles) = api.profiles() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if profiles.contains(&PROFILE_HEVC_MAIN) && api.supports(PROFILE_HEVC_MAIN, entry) {
        out.push(CodecId::Hevc);
    }
    let h264 = [PROFILE_H264_HIGH, PROFILE_H264_MAIN]
        .into_iter()
        .find(|profile| profiles.contains(profile) && api.supports(*profile, entry));
    if h264.is_some() {
        out.push(CodecId::H264);
    }
    out
}

/// Hardware encoders, HEVC first when VA-API has an EncSlice entrypoint.
pub fn hardware_encode_codecs() -> Vec<CodecId> {
    codecs_for(ENTRY_ENC_SLICE)
}

/// Hardware decoders, HEVC first when VA-API has a VLD entrypoint.
pub fn hardware_decode_codecs() -> Vec<CodecId> {
    codecs_for(ENTRY_VLD)
}

pub fn require_encode_codec() -> Result<Vec<CodecId>, PlatformError> {
    let codecs = hardware_encode_codecs();
    if codecs.is_empty() {
        return Err(PlatformError::new(
            "this machine has no hardware H.264 or HEVC encoder",
        ));
    }
    Ok(codecs)
}

pub fn even(value: u32) -> u32 {
    let value = value.max(2);
    value - (value % 2)
}

pub fn align_up(value: u32, align: u32) -> u32 {
    let align = align.max(1);
    value.div_ceil(align) * align
}

/// Length-prefixed NALs. `nal_length_size` is 1, 2, or 4.
pub fn split_avcc(payload: &[u8], nal_length_size: u8) -> Result<Vec<Vec<u8>>, PlatformError> {
    let width = nal_length_size as usize;
    if width == 0 || width > 4 {
        return Err(PlatformError::new("NAL length size is not 1, 2, or 4"));
    }
    let mut out = Vec::new();
    let mut index = 0;
    while index + width <= payload.len() {
        let mut len = 0usize;
        for byte in &payload[index..index + width] {
            len = (len << 8) | usize::from(*byte);
        }
        index += width;
        if len == 0 || index + len > payload.len() {
            break;
        }
        out.push(payload[index..index + len].to_vec());
        index += len;
    }
    Ok(out)
}

pub fn annex_b_nals(data: &[u8]) -> Vec<Vec<u8>> {
    let mut marks = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            marks.push(index + 3);
            index += 3;
        } else if index + 4 <= data.len() && data[index..index + 4] == [0, 0, 0, 1] {
            marks.push(index + 4);
            index += 4;
        } else {
            index += 1;
        }
    }
    if marks.is_empty() {
        return if data.len() > 1 { vec![data.to_vec()] } else { Vec::new() };
    }
    let mut nals = Vec::new();
    for (slot, start) in marks.iter().copied().enumerate() {
        let end = if slot + 1 < marks.len() {
            let next = marks[slot + 1];
            if next >= 4 && data[next - 4..next] == [0, 0, 0, 1] {
                next - 4
            } else if next >= 3 {
                next - 3
            } else {
                next
            }
        } else {
            data.len()
        };
        if end > start {
            nals.push(data[start..end].to_vec());
        }
    }
    nals
}

pub fn h264_nal_type(nal: &[u8]) -> u8 {
    nal.first().copied().unwrap_or(0) & 0x1f
}

pub fn hevc_nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|byte| (byte >> 1) & 0x3f).unwrap_or(0)
}

pub fn avcc(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        let len = u32::try_from(nal.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

pub fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32, coded_w: u32, coded_h: u32, pitches: [u32; 3], offsets: [u32; 3], dest: &mut [u8]) {
    let width = width.min(coded_w);
    let height = height.min(coded_h);
    let y_pitch = pitches[0].max(coded_w) as usize;
    let uv_pitch = pitches[1].max(coded_w) as usize;
    let y_off = offsets[0] as usize;
    let uv_off = offsets[1] as usize;
    for y in 0..coded_h {
        let sy = y.min(height.saturating_sub(1));
        for x in 0..coded_w {
            let sx = x.min(width.saturating_sub(1));
            let src = ((sy * width + sx) * 4) as usize;
            if src + 2 >= bgra.len() {
                continue;
            }
            let b = i32::from(bgra[src]);
            let g = i32::from(bgra[src + 1]);
            let r = i32::from(bgra[src + 2]);
            let luma = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            let at = y_off + y as usize * y_pitch + x as usize;
            if at < dest.len() {
                dest[at] = luma.clamp(0, 255) as u8;
            }
        }
    }
    for y in (0..coded_h).step_by(2) {
        let sy = y.min(height.saturating_sub(1));
        for x in (0..coded_w).step_by(2) {
            let sx = x.min(width.saturating_sub(1));
            let mut u_acc = 0i32;
            let mut v_acc = 0i32;
            let mut n = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let py = sy.saturating_add(dy).min(height.saturating_sub(1));
                    let px = sx.saturating_add(dx).min(width.saturating_sub(1));
                    let src = ((py * width + px) * 4) as usize;
                    if src + 2 >= bgra.len() {
                        continue;
                    }
                    let b = i32::from(bgra[src]);
                    let g = i32::from(bgra[src + 1]);
                    let r = i32::from(bgra[src + 2]);
                    u_acc += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                    v_acc += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let at = uv_off + (y as usize / 2) * uv_pitch + x as usize;
            if at + 1 < dest.len() {
                dest[at] = (u_acc / n).clamp(0, 255) as u8;
                dest[at + 1] = (v_acc / n).clamp(0, 255) as u8;
            }
        }
    }
}

pub fn nv12_to_bgra(src: &[u8], width: u32, height: u32, pitches: [u32; 3], offsets: [u32; 3]) -> Vec<u8> {
    let mut out = vec![0u8; width as usize * height as usize * 4];
    let y_pitch = pitches[0].max(width) as usize;
    let uv_pitch = pitches[1].max(width) as usize;
    let y_off = offsets[0] as usize;
    let uv_off = offsets[1] as usize;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let y_at = y_off + y * y_pitch + x;
            let uv_at = uv_off + (y / 2) * uv_pitch + (x & !1);
            if y_at >= src.len() || uv_at + 1 >= src.len() {
                continue;
            }
            let luma = i32::from(src[y_at]) - 16;
            let u = i32::from(src[uv_at]) - 128;
            let v = i32::from(src[uv_at + 1]) - 128;
            let c = 298 * luma.max(0);
            let r = (c + 409 * v + 128) >> 8;
            let g = (c - 100 * u - 208 * v + 128) >> 8;
            let b = (c + 516 * u + 128) >> 8;
            let dst = (y * width as usize + x) * 4;
            out[dst] = b.clamp(0, 255) as u8;
            out[dst + 1] = g.clamp(0, 255) as u8;
            out[dst + 2] = r.clamp(0, 255) as u8;
            out[dst + 3] = 255;
        }
    }
    out
}

pub fn copy_bgra_rows(src: &[u8], width: u32, height: u32, pitch: usize) -> Vec<u8> {
    let mut out = vec![0u8; width as usize * height as usize * 4];
    let row = width as usize * 4;
    for y in 0..height as usize {
        let from = y * pitch;
        let to = y * row;
        if from + row <= src.len() && to + row <= out.len() {
            out[to..to + row].copy_from_slice(&src[from..from + row]);
        }
    }
    out
}

pub fn image_format(fourcc: u32, bpp: u32) -> ImageFormat {
    ImageFormat {
        fourcc,
        byte_order: VA_LSB_FIRST,
        bits_per_pixel: bpp,
        depth: bpp.min(32),
        red_mask: 0,
        green_mask: 0,
        blue_mask: 0,
        alpha_mask: 0,
        va_reserved: [0; 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_dimensions_drop_the_odd_pixel() {
        assert_eq!(even(1920), 1920);
        assert_eq!(even(1081), 1080);
        assert_eq!(even(1), 2);
    }

    #[test]
    fn avcc_lengths_round_trip() {
        let nals = vec![vec![0x67, 0x42], vec![0x68, 0xce]];
        let bytes = avcc(&nals);
        assert_eq!(split_avcc(&bytes, 4).unwrap(), nals);
    }
}
