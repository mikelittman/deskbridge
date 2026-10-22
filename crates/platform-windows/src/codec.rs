//! Hardware Media Foundation encoder and decoder discovery.
//!
//! Probes call `CoInitializeEx` and `MFStartup`. A missing Media Foundation
//! install yields an empty list instead of a panic.

use std::mem::ManuallyDrop;

use windows::core::{GUID, HRESULT};
use windows::Win32::Media::MediaFoundation::{
    AACMFTEncoder, CMSAACDecMFT, IMFActivate, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform,
    MFAudioFormat_AAC, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Audio, MFMediaType_Video, MFShutdown, MFStartup, MFTEnumEx, MFT_CATEGORY_AUDIO_DECODER,
    MFT_CATEGORY_AUDIO_ENCODER, MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_REGISTER_TYPE_INFO, MFSTARTUP_FULL, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoFormat_RGB32, MFVideoInterlace_Progressive,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_USER_DATA,
    MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

use deskbridge_proto::CodecId;

use crate::error::PlatformError;

const RPC_E_CHANGED_MODE: HRESULT = HRESULT(0x80010106u32 as i32);

/// Keeps Media Foundation started for as long as an encoder, decoder, or probe holds it.
pub(crate) struct MfGuard {
    com: bool,
    winrt: bool,
}

impl MfGuard {
    pub(crate) fn startup() -> Result<Self, PlatformError> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let com = if hr.is_ok() {
            true
        } else if hr == RPC_E_CHANGED_MODE {
            false
        } else {
            return Err(PlatformError::new(format!("COM did not start ({})", hr.0)));
        };
        let winrt = match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => true,
            Err(err) if err.code() == RPC_E_CHANGED_MODE => false,
            Err(_) => false,
        };
        if let Err(err) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            if com {
                unsafe { CoUninitialize() };
            }
            if winrt {
                unsafe { RoUninitialize() };
            }
            return Err(PlatformError::new(format!("Media Foundation did not start ({err})")));
        }
        Ok(Self { com, winrt })
    }
}

impl Drop for MfGuard {
    fn drop(&mut self) {
        unsafe { MFShutdown().ok() };
        if self.winrt {
            unsafe { RoUninitialize() };
        }
        if self.com {
            unsafe { CoUninitialize() };
        }
    }
}

pub fn hardware_encode_codecs() -> Vec<CodecId> {
    std::panic::catch_unwind(probe_encode).unwrap_or_default()
}

pub fn hardware_decode_codecs() -> Vec<CodecId> {
    std::panic::catch_unwind(probe_decode).unwrap_or_default()
}

pub fn require_encode_codec() -> Result<Vec<CodecId>, PlatformError> {
    let codecs = hardware_encode_codecs();
    if codecs.is_empty() {
        return Err(PlatformError::new("no hardware H.264 or HEVC encoder"));
    }
    Ok(codecs)
}

fn probe_encode() -> Vec<CodecId> {
    let Ok(_mf) = MfGuard::startup() else {
        return Vec::new();
    };
    let mut codecs = Vec::new();
    if hardware_mft_exists(MFT_CATEGORY_VIDEO_ENCODER, None, Some(MFVideoFormat_HEVC)) {
        codecs.push(CodecId::Hevc);
    }
    if hardware_mft_exists(MFT_CATEGORY_VIDEO_ENCODER, None, Some(MFVideoFormat_H264)) {
        codecs.push(CodecId::H264);
    }
    codecs
}

fn probe_decode() -> Vec<CodecId> {
    let Ok(_mf) = MfGuard::startup() else {
        return Vec::new();
    };
    let mut codecs = Vec::new();
    if hardware_mft_exists(MFT_CATEGORY_VIDEO_DECODER, Some(MFVideoFormat_HEVC), None) {
        codecs.push(CodecId::Hevc);
    }
    if hardware_mft_exists(MFT_CATEGORY_VIDEO_DECODER, Some(MFVideoFormat_H264), None) {
        codecs.push(CodecId::H264);
    }
    codecs
}

fn hardware_mft_exists(category: GUID, input: Option<GUID>, output: Option<GUID>) -> bool {
    !enumerate_mfts(category, input, output).is_empty()
}

pub(crate) fn enumerate_mfts(category: GUID, input: Option<GUID>, output: Option<GUID>) -> Vec<IMFActivate> {
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    enumerate_mfts_in(category, MFMediaType_Video, input, output, flags)
}

fn enumerate_audio_mfts(category: GUID, input: Option<GUID>, output: Option<GUID>) -> Vec<IMFActivate> {
    let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    enumerate_mfts_in(category, MFMediaType_Audio, input, output, flags)
}

fn enumerate_mfts_in(
    category: GUID,
    major: GUID,
    input: Option<GUID>,
    output: Option<GUID>,
    flags: MFT_ENUM_FLAG,
) -> Vec<IMFActivate> {
    let input_info = input.map(|subtype| MFT_REGISTER_TYPE_INFO {
        guidMajorType: major,
        guidSubtype: subtype,
    });
    let output_info = output.map(|subtype| MFT_REGISTER_TYPE_INFO {
        guidMajorType: major,
        guidSubtype: subtype,
    });
    let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let enumerated = unsafe {
        MFTEnumEx(
            category,
            flags,
            input_info.as_ref().map(|info| info as *const _),
            output_info.as_ref().map(|info| info as *const _),
            &mut raw,
            &mut count,
        )
    };
    if enumerated.is_err() || raw.is_null() || count == 0 {
        if !raw.is_null() {
            unsafe { CoTaskMemFree(Some(raw.cast())) };
        }
        return Vec::new();
    }
    let found = unsafe {
        let slots = std::slice::from_raw_parts_mut(raw, count as usize);
        let list: Vec<IMFActivate> = slots.iter_mut().filter_map(|slot| slot.take()).collect();
        CoTaskMemFree(Some(raw.cast()));
        list
    };
    found
}

pub(crate) fn video_subtype(codec: CodecId) -> GUID {
    match codec {
        CodecId::H264 => MFVideoFormat_H264,
        CodecId::Hevc => MFVideoFormat_HEVC,
    }
}

pub(crate) fn pack_size(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

#[derive(Clone, Copy)]
pub(crate) enum PixelLayout {
    Rgb32,
    Nv12,
}

pub(crate) fn make_video_type(
    subtype: &GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: Option<u32>,
) -> Result<IMFMediaType, PlatformError> {
    let media = unsafe { MFCreateMediaType() }.map_err(PlatformError::new)?;
    unsafe {
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(PlatformError::new)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype).map_err(PlatformError::new)?;
        media
            .SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))
            .map_err(PlatformError::new)?;
        media
            .SetUINT64(&MF_MT_FRAME_RATE, pack_size(fps.max(1), 1))
            .map_err(PlatformError::new)?;
        media
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_size(1, 1))
            .map_err(PlatformError::new)?;
        media
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(PlatformError::new)?;
        if let Some(bitrate) = bitrate {
            media.SetUINT32(&MF_MT_AVG_BITRATE, bitrate.max(1)).map_err(PlatformError::new)?;
        }
    }
    Ok(media)
}

/// AAC uses the 2-byte AudioSpecificConfig on the wire. Media Foundation wants that
/// config after the 12-byte tail of `HEAACWAVEINFO`. Payload type 0 is raw AAC.
pub(crate) fn make_audio_type(
    subtype: GUID,
    rate: u32,
    channels: u8,
    bits: u32,
    avg_bytes: u32,
    asc: Option<&[u8]>,
) -> Result<IMFMediaType, PlatformError> {
    let media = unsafe { MFCreateMediaType() }.map_err(PlatformError::new)?;
    let channels = u32::from(channels.max(1));
    let block = if subtype == MFAudioFormat_AAC {
        1
    } else {
        (channels * bits.max(8) / 8).max(1)
    };
    unsafe {
        media
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(PlatformError::new)?;
        media.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(PlatformError::new)?;
        media
            .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, bits)
            .map_err(PlatformError::new)?;
        media
            .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate.max(1))
            .map_err(PlatformError::new)?;
        media
            .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)
            .map_err(PlatformError::new)?;
        media
            .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bytes.max(1))
            .map_err(PlatformError::new)?;
        media
            .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block)
            .map_err(PlatformError::new)?;
        if subtype == MFAudioFormat_AAC {
            media.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(PlatformError::new)?;
            media
                .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
                .map_err(PlatformError::new)?;
            let asc = asc.unwrap_or(&[]);
            let mut user = vec![0u8; 12 + asc.len()];
            user[2] = 0x29;
            user[12..].copy_from_slice(asc);
            media.SetBlob(&MF_MT_USER_DATA, &user).map_err(PlatformError::new)?;
        }
    }
    Ok(media)
}

/// The system AAC encoder and decoder are software transforms, so a hardware-only search misses them.
pub(crate) fn open_aac_transform(encode: bool) -> Result<IMFTransform, PlatformError> {
    let clsid = if encode { &AACMFTEncoder } else { &CMSAACDecMFT };
    let created: Result<IMFTransform, _> =
        unsafe { CoCreateInstance(clsid, None, CLSCTX_INPROC_SERVER) };
    if let Ok(transform) = created {
        return Ok(transform);
    }
    let (category, input, output) = if encode {
        (MFT_CATEGORY_AUDIO_ENCODER, None, Some(MFAudioFormat_AAC))
    } else {
        (MFT_CATEGORY_AUDIO_DECODER, Some(MFAudioFormat_AAC), None)
    };
    let activates = enumerate_audio_mfts(category, input, output);
    if let Some(activate) = activates.first() {
        if let Ok(transform) = unsafe { activate.ActivateObject() } {
            return Ok(transform);
        }
    }
    Err(PlatformError::new(if encode {
        "no AAC encoder"
    } else {
        "no AAC decoder"
    }))
}

pub(crate) fn input_subtype(layout: PixelLayout) -> GUID {
    match layout {
        PixelLayout::Rgb32 => MFVideoFormat_RGB32,
        PixelLayout::Nv12 => MFVideoFormat_NV12,
    }
}

/// Media Foundation `RGB32` is BGRA in memory.
pub(crate) fn bgra_to_nv12(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let width = width as usize;
    let height = height as usize;
    let mut dst = vec![0u8; width * height * 3 / 2];
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 4;
            if i + 2 >= src.len() {
                return dst;
            }
            let b = i32::from(src[i]);
            let g = i32::from(src[i + 1]);
            let r = i32::from(src[i + 2]);
            let luma = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            dst[y * width + x] = luma.clamp(0, 255) as u8;
        }
    }
    let y_size = width * height;
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let mut r = 0i32;
            let mut g = 0i32;
            let mut b = 0i32;
            let mut n = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let yy = (y + dy).min(height - 1);
                    let xx = (x + dx).min(width - 1);
                    let i = (yy * width + xx) * 4;
                    if i + 2 >= src.len() {
                        continue;
                    }
                    b += i32::from(src[i]);
                    g += i32::from(src[i + 1]);
                    r += i32::from(src[i + 2]);
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            r /= n;
            g /= n;
            b /= n;
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            let uv = y_size + (y / 2) * width + x;
            if uv + 1 < dst.len() {
                dst[uv] = u.clamp(0, 255) as u8;
                dst[uv + 1] = v.clamp(0, 255) as u8;
            }
        }
    }
    dst
}

pub(crate) fn nv12_to_bgra(src: &[u8], width: u32, height: u32, y_stride: usize, uv_stride: usize) -> Vec<u8> {
    let width = width as usize;
    let height = height as usize;
    let mut dst = vec![0u8; width * height * 4];
    let y_plane = 0usize;
    let uv_plane = y_stride.saturating_mul(height);
    for y in 0..height {
        for x in 0..width {
            let yi = y_plane + y * y_stride + x;
            let uv = uv_plane + (y / 2) * uv_stride + (x & !1);
            if yi >= src.len() || uv + 1 >= src.len() {
                continue;
            }
            let yy = i32::from(src[yi]) - 16;
            let uu = i32::from(src[uv]) - 128;
            let vv = i32::from(src[uv + 1]) - 128;
            let r = ((298 * yy + 409 * vv + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * yy - 100 * uu - 208 * vv + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * yy + 516 * uu + 128) >> 8).clamp(0, 255) as u8;
            let i = (y * width + x) * 4;
            dst[i] = b;
            dst[i + 1] = g;
            dst[i + 2] = r;
            dst[i + 3] = 255;
        }
    }
    dst
}

pub(crate) fn rgb32_to_bgra(src: &[u8], width: u32, height: u32, stride: usize) -> Vec<u8> {
    let row = width as usize * 4;
    let mut dst = vec![0u8; row * height as usize];
    for y in 0..height as usize {
        let s = y * stride;
        let d = y * row;
        if s + row <= src.len() && d + row <= dst.len() {
            dst[d..d + row].copy_from_slice(&src[s..s + row]);
            for x in 0..width as usize {
                dst[d + x * 4 + 3] = 255;
            }
        }
    }
    dst
}

pub(crate) fn copy_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, PlatformError> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(PlatformError::new)?;
    lock_buffer(&buffer)
}

fn lock_buffer(buffer: &IMFMediaBuffer) -> Result<Vec<u8>, PlatformError> {
    let mut data = std::ptr::null_mut();
    let mut current = 0u32;
    unsafe { buffer.Lock(&mut data, None, Some(&mut current)) }.map_err(PlatformError::new)?;
    let bytes = if data.is_null() || current == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data, current as usize).to_vec() }
    };
    unsafe { buffer.Unlock().ok() };
    Ok(bytes)
}

pub(crate) fn make_sample(bytes: &[u8]) -> Result<IMFSample, PlatformError> {
    let sample = unsafe { MFCreateSample() }.map_err(PlatformError::new)?;
    let buffer = unsafe { MFCreateMemoryBuffer(bytes.len() as u32) }.map_err(PlatformError::new)?;
    let mut data = std::ptr::null_mut();
    unsafe { buffer.Lock(&mut data, None, None) }.map_err(PlatformError::new)?;
    if !data.is_null() && !bytes.is_empty() {
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len()) };
    }
    unsafe {
        buffer.Unlock().ok();
        buffer.SetCurrentLength(bytes.len() as u32).map_err(PlatformError::new)?;
        sample.AddBuffer(&buffer).map_err(PlatformError::new)?;
    }
    Ok(sample)
}

pub(crate) fn take_output_sample(slot: &mut ManuallyDrop<Option<IMFSample>>) -> Option<IMFSample> {
    unsafe { ManuallyDrop::take(slot) }
}

pub(crate) fn h264_nal_type(nal: &[u8]) -> u8 {
    nal.first().copied().unwrap_or(0) & 0x1f
}

pub(crate) fn hevc_nal_type(nal: &[u8]) -> u8 {
    (nal.first().copied().unwrap_or(0) >> 1) & 0x3f
}

pub(crate) fn is_idr(codec: CodecId, nal: &[u8]) -> bool {
    match codec {
        CodecId::H264 => h264_nal_type(nal) == 5,
        CodecId::Hevc => matches!(hevc_nal_type(nal), 19 | 20 | 21),
    }
}

pub(crate) fn is_parameter_nal(codec: CodecId, nal: &[u8]) -> bool {
    match codec {
        CodecId::H264 => matches!(h264_nal_type(nal), 7 | 8),
        CodecId::Hevc => matches!(hevc_nal_type(nal), 32 | 33 | 34),
    }
}

/// Split Annex-B (start codes) into NALs without the start codes.
pub(crate) fn split_annex_b(data: &[u8]) -> Vec<Vec<u8>> {
    let mut marks = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let prefix = if i > 0 && data[i - 1] == 0 { 4 } else { 3 };
            let at = if prefix == 4 { i - 1 } else { i };
            marks.push((at, prefix));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (index, (at, prefix)) in marks.iter().copied().enumerate() {
        let start = at + prefix;
        let end = marks.get(index + 1).map(|(next, _)| *next).unwrap_or(data.len());
        if start < end {
            let nal = &data[start..end];
            let nal = trim_trailing_zeros(nal);
            if !nal.is_empty() {
                nals.push(nal.to_vec());
            }
        }
    }
    nals
}

fn trim_trailing_zeros(nal: &[u8]) -> &[u8] {
    let mut end = nal.len();
    while end > 0 && nal[end - 1] == 0 {
        end -= 1;
    }
    &nal[..end]
}

pub(crate) fn looks_like_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

pub(crate) fn nals_to_avcc(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

pub(crate) fn avcc_to_annex_b(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if len == 0 || i + len > data.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..i + len]);
        i += len;
    }
    out
}

pub(crate) fn payload_nals(codec: CodecId, data: &[u8]) -> Vec<Vec<u8>> {
    let _ = codec;
    if data.is_empty() {
        return Vec::new();
    }
    if looks_like_annex_b(data) {
        split_annex_b(data)
    } else {
        split_avcc(data)
    }
}

fn split_avcc(data: &[u8]) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if len == 0 || i + len > data.len() {
            break;
        }
        nals.push(data[i..i + len].to_vec());
        i += len;
    }
    nals
}

/// `MF_MT_MPEG_SEQUENCE_HEADER` is an AVC or HEVC decoder configuration record,
/// or occasionally Annex-B. Either way, return the parameter-set NALs.
pub(crate) fn parameter_sets_from_header(codec: CodecId, header: &[u8]) -> Vec<Vec<u8>> {
    if header.is_empty() {
        return Vec::new();
    }
    if looks_like_annex_b(header) {
        return split_annex_b(header)
            .into_iter()
            .filter(|nal| is_parameter_nal(codec, nal))
            .collect();
    }
    match codec {
        CodecId::H264 => parse_avcc(header),
        CodecId::Hevc => parse_hvcc(header),
    }
}

fn parse_avcc(header: &[u8]) -> Vec<Vec<u8>> {
    if header.len() < 7 {
        return Vec::new();
    }
    let mut i = 5;
    let mut sets = Vec::new();
    let sps_count = header[i] & 0x1f;
    i += 1;
    for _ in 0..sps_count {
        if let Some((nal, next)) = read_be_nal(header, i) {
            sets.push(nal);
            i = next;
        }
    }
    if i >= header.len() {
        return sets;
    }
    let pps_count = header[i];
    i += 1;
    for _ in 0..pps_count {
        if let Some((nal, next)) = read_be_nal(header, i) {
            sets.push(nal);
            i = next;
        }
    }
    sets
}

fn parse_hvcc(header: &[u8]) -> Vec<Vec<u8>> {
    // Skip the fixed prefix of HEVCDecoderConfigurationRecord (23 bytes).
    if header.len() < 23 {
        return Vec::new();
    }
    let mut i = 22;
    let arrays = header[i] as usize;
    i += 1;
    let mut sets = Vec::new();
    for _ in 0..arrays {
        if i + 3 > header.len() {
            break;
        }
        i += 1;
        let count = u16::from_be_bytes([header[i], header[i + 1]]) as usize;
        i += 2;
        for _ in 0..count {
            if let Some((nal, next)) = read_be_nal(header, i) {
                sets.push(nal);
                i = next;
            } else {
                return sets;
            }
        }
    }
    sets
}

fn read_be_nal(data: &[u8], i: usize) -> Option<(Vec<u8>, usize)> {
    if i + 2 > data.len() {
        return None;
    }
    let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
    let start = i + 2;
    if len == 0 || start + len > data.len() {
        return None;
    }
    Some((data[start..start + len].to_vec(), start + len))
}

pub(crate) fn prepend_parameter_sets(sets: &[Vec<u8>], annex_b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for set in sets {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(set);
    }
    out.extend_from_slice(annex_b);
    out
}

pub(crate) fn activate_first(activates: &[IMFActivate]) -> Result<IMFTransform, PlatformError> {
    let activate = activates
        .first()
        .ok_or_else(|| PlatformError::new("no hardware Media Foundation transform"))?;
    unsafe { activate.ActivateObject() }.map_err(PlatformError::new)
}

pub(crate) fn even(value: u32) -> u32 {
    let value = value.max(2);
    value - (value % 2)
}

/// Fit inside `max_w`×`max_h`. Aspect is kept, nothing is scaled up, both sides stay even.
pub(crate) fn fit_inside(native_w: u32, native_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let native_w = even(native_w);
    let native_h = even(native_h);
    let max_w = even(max_w.max(2));
    let max_h = even(max_h.max(2));
    if native_w <= max_w && native_h <= max_h {
        return (native_w, native_h);
    }
    let scale = (f64::from(max_w) / f64::from(native_w)).min(f64::from(max_h) / f64::from(native_h));
    let width = even((f64::from(native_w) * scale).floor() as u32).min(max_w);
    let height = even((f64::from(native_h) * scale).floor() as u32).min(max_h);
    (width.max(2), height.max(2))
}

pub(crate) fn scale_bgra(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let sw = sw.max(1);
    let sh = sh.max(1);
    let dw = even(dw);
    let dh = even(dh);
    let src_row = sw as usize * 4;
    if sw == dw && sh == dh && src.len() >= src_row * sh as usize {
        return src[..src_row * sh as usize].to_vec();
    }
    let mut dst = vec![0u8; dw as usize * dh as usize * 4];
    for y in 0..dh {
        let fy = (f64::from(y) + 0.5) * f64::from(sh) / f64::from(dh) - 0.5;
        let y0 = fy.floor().clamp(0.0, f64::from(sh - 1)) as u32;
        let y1 = (y0 + 1).min(sh - 1);
        let ty = (fy - f64::from(y0)).clamp(0.0, 1.0) as f32;
        for x in 0..dw {
            let fx = (f64::from(x) + 0.5) * f64::from(sw) / f64::from(dw) - 0.5;
            let x0 = fx.floor().clamp(0.0, f64::from(sw - 1)) as u32;
            let x1 = (x0 + 1).min(sw - 1);
            let tx = (fx - f64::from(x0)).clamp(0.0, 1.0) as f32;
            let mut pixel = [0u8; 4];
            for channel in 0..4 {
                let sample = |xx: u32, yy: u32| -> f32 {
                    let i = (yy as usize * sw as usize + xx as usize) * 4 + channel;
                    src.get(i).copied().unwrap_or(0) as f32
                };
                let v00 = sample(x0, y0);
                let v10 = sample(x1, y0);
                let v01 = sample(x0, y1);
                let v11 = sample(x1, y1);
                let top = v00 + (v10 - v00) * tx;
                let bottom = v01 + (v11 - v01) * tx;
                pixel[channel] = (top + (bottom - top) * ty).round().clamp(0.0, 255.0) as u8;
            }
            let i = (y as usize * dw as usize + x as usize) * 4;
            dst[i..i + 4].copy_from_slice(&pixel);
        }
    }
    dst
}

pub(crate) fn sample_hash(bgra: &[u8], width: u32, height: u32) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    if width == 0 || height == 0 {
        return hash;
    }
    let step_x = (width / 32).max(1);
    let step_y = (height / 32).max(1);
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let i = ((y * width + x) * 4) as usize;
            if i + 3 < bgra.len() {
                let pixel = u32::from_le_bytes([bgra[i], bgra[i + 1], bgra[i + 2], bgra[i + 3]]);
                hash = hash.wrapping_mul(0x100000001b3) ^ u64::from(pixel);
            }
            x += step_x;
        }
        y += step_y;
    }
    hash
}
