use std::mem::ManuallyDrop;

use windows::Win32::Media::MediaFoundation::{
    IMFTransform, MFVideoFormat_NV12, MFVideoFormat_RGB32, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_MT_FRAME_SIZE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK,
    MFT_CATEGORY_VIDEO_DECODER, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
};

use deskbridge_proto::{CodecConfig, CodecId, VideoFrame};

use crate::codec::{
    activate_first, avcc_to_annex_b, enumerate_mfts, make_sample, make_video_type, nv12_to_bgra, pack_size,
    prepend_parameter_sets, rgb32_to_bgra, video_subtype, MfGuard,
};
use crate::error::PlatformError;

pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub struct Decoder {
    transform: Option<IMFTransform>,
    codec: Option<CodecId>,
    parameter_sets: Vec<Vec<u8>>,
    width: u32,
    height: u32,
    nv12: bool,
    provides_samples: bool,
    _mf: Option<MfGuard>,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            transform: None,
            codec: None,
            parameter_sets: Vec::new(),
            width: 0,
            height: 0,
            nv12: true,
            provides_samples: true,
            _mf: None,
        }
    }

    pub fn decode(&mut self, config: &CodecConfig, frame: &VideoFrame) -> Result<Option<Picture>, PlatformError> {
        if frame.payload.is_empty() {
            return Ok(None);
        }
        if self.codec != Some(config.codec)
            || self.parameter_sets != config.parameter_sets
            || self.width != config.width
            || self.height != config.height
        {
            self.open(config)?;
        }
        let transform = self
            .transform
            .as_ref()
            .ok_or_else(|| PlatformError::new("decoder is not open"))?;
        let mut annex = avcc_to_annex_b(&frame.payload);
        if annex.is_empty() {
            return Ok(None);
        }
        if frame.keyframe {
            annex = prepend_parameter_sets(&self.parameter_sets, &annex);
        }
        let sample = make_sample(&annex)?;
        unsafe { transform.ProcessInput(0, &sample, 0) }.map_err(PlatformError::new)?;
        self.drain()
    }

    fn open(&mut self, config: &CodecConfig) -> Result<(), PlatformError> {
        let mf = MfGuard::startup()?;
        let activates = enumerate_mfts(MFT_CATEGORY_VIDEO_DECODER, Some(video_subtype(config.codec)), None);
        if activates.is_empty() {
            return Err(PlatformError::new(format!(
                "no hardware {} decoder",
                config.codec.name()
            )));
        }
        let transform = activate_first(&activates)?;
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            let _ = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
        }
        let header = sequence_header(config);
        let input = make_video_type(&video_subtype(config.codec), config.width.max(2), config.height.max(2), 30, None)?;
        if !header.is_empty() {
            unsafe { input.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &header).ok() };
        }
        unsafe { transform.SetInputType(0, &input, 0) }.map_err(PlatformError::new)?;
        let nv12 = select_output(&transform, config.width, config.height)?;
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ok();
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ok();
        }
        let info = unsafe { transform.GetOutputStreamInfo(0) }.unwrap_or_default();
        self.provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0 || info.cbSize == 0;
        self.transform = Some(transform);
        self.codec = Some(config.codec);
        self.parameter_sets = config.parameter_sets.clone();
        self.width = config.width.max(2);
        self.height = config.height.max(2);
        self.nv12 = nv12;
        self._mf = Some(mf);
        Ok(())
    }

    fn drain(&mut self) -> Result<Option<Picture>, PlatformError> {
        let Some(transform) = self.transform.as_ref() else {
            return Ok(None);
        };
        let mut picture = None;
        for _ in 0..8 {
            let mut buffer = MFT_OUTPUT_DATA_BUFFER::default();
            if !self.provides_samples {
                let bytes = self.width.saturating_mul(self.height).saturating_mul(4).max(64 * 1024);
                buffer.pSample = ManuallyDrop::new(Some(make_sample(&vec![0u8; bytes as usize])?));
            }
            let mut status = 0u32;
            let result = unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
            let sample = crate::codec::take_output_sample(&mut buffer.pSample);
            let _events = unsafe { ManuallyDrop::take(&mut buffer.pEvents) };
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        if let Ok(bytes) = crate::codec::copy_sample_bytes(&sample) {
                            if !bytes.is_empty() {
                                picture = Some(self.to_bgra(&bytes)?);
                            }
                        }
                    }
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let _ = select_output(transform, self.width, self.height);
                }
                Err(err) => return Err(PlatformError::new(err)),
            }
        }
        Ok(picture)
    }

    fn to_bgra(&self, bytes: &[u8]) -> Result<Picture, PlatformError> {
        let width = self.width.max(1);
        let height = self.height.max(1);
        let bgra = if self.nv12 {
            let y_stride = width as usize;
            let uv_stride = width as usize;
            if bytes.len() < y_stride * height as usize {
                return Err(PlatformError::new("decoded NV12 buffer is short"));
            }
            nv12_to_bgra(bytes, width, height, y_stride, uv_stride)
        } else {
            rgb32_to_bgra(bytes, width, height, width as usize * 4)
        };
        Ok(Picture { width, height, bgra })
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

fn select_output(transform: &IMFTransform, width: u32, height: u32) -> Result<bool, PlatformError> {
    let mut fallback = None;
    for index in 0..16u32 {
        let Ok(available) = (unsafe { transform.GetOutputAvailableType(0, index) }) else {
            break;
        };
        let Ok(subtype) = (unsafe { available.GetGUID(&MF_MT_SUBTYPE) }) else {
            continue;
        };
        unsafe { available.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width.max(2), height.max(2))).ok() };
        let nv12 = subtype == MFVideoFormat_NV12;
        let rgb = subtype == MFVideoFormat_RGB32;
        if !nv12 && !rgb {
            continue;
        }
        if unsafe { transform.SetOutputType(0, &available, 0) }.is_ok() {
            if rgb {
                return Ok(false);
            }
            fallback = Some(true);
        }
    }
    if fallback == Some(true) {
        return Ok(true);
    }
    for subtype in [MFVideoFormat_RGB32, MFVideoFormat_NV12] {
        let output = make_video_type(&subtype, width.max(2), height.max(2), 30, None)?;
        if unsafe { transform.SetOutputType(0, &output, 0) }.is_ok() {
            return Ok(subtype == MFVideoFormat_NV12);
        }
    }
    Err(PlatformError::new("hardware decoder has no BGRA or NV12 output"))
}

fn sequence_header(config: &CodecConfig) -> Vec<u8> {
    match config.codec {
        CodecId::H264 => avcc_record(&config.parameter_sets),
        CodecId::Hevc => hvcc_record(&config.parameter_sets),
    }
}

fn avcc_record(sets: &[Vec<u8>]) -> Vec<u8> {
    let sps: Vec<&[u8]> = sets.iter().map(Vec::as_slice).filter(|nal| crate::codec::h264_nal_type(nal) == 7).collect();
    let pps: Vec<&[u8]> = sets.iter().map(Vec::as_slice).filter(|nal| crate::codec::h264_nal_type(nal) == 8).collect();
    if sps.is_empty() {
        return Vec::new();
    }
    let profile = sps[0].get(1).copied().unwrap_or(0x42);
    let compat = sps[0].get(2).copied().unwrap_or(0);
    let level = sps[0].get(3).copied().unwrap_or(0x1f);
    let mut out = vec![1, profile, compat, level, 0xff, 0xe0 | (sps.len() as u8 & 0x1f)];
    for nal in sps {
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out.push(pps.len() as u8);
    for nal in pps {
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

fn hvcc_record(sets: &[Vec<u8>]) -> Vec<u8> {
    if sets.is_empty() {
        return Vec::new();
    }
    // A minimal HEVCDecoderConfigurationRecord. General profile bytes are placeholders;
    // the NAL arrays are what the decoder needs to start.
    let mut out = vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xf0];
    out[0] = 1;
    out.push(sets.len().min(255) as u8);
    for nal in sets.iter().take(255) {
        let kind = crate::codec::hevc_nal_type(nal);
        out.push(kind);
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}
