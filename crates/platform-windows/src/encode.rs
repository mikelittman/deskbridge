use std::mem::ManuallyDrop;

use windows::core::{Interface, VARIANT};
use windows::Win32::Media::MediaFoundation::{
    IMFSample, IMFTransform, MFSampleExtension_CleanPoint, MFVideoInterlace_Progressive, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncVideoForceKeyFrame,
    CODECAPI_AVLowLatencyMode, ICodecAPI, eAVEncCommonRateControlMode_CBR,
};

use deskbridge_proto::{CodecConfig, CodecId, VideoFrame};
use deskbridge_session::mono_us;

use crate::codec::{
    activate_first, bgra_to_nv12, enumerate_mfts, even, input_subtype, is_idr, is_parameter_nal, make_sample,
    make_video_type, nals_to_avcc, pack_size, parameter_sets_from_header, payload_nals, video_subtype, MfGuard,
    PixelLayout,
};
use crate::error::PlatformError;

const MAX_GOP: u32 = 3600;

pub struct Encoded {
    pub frame: VideoFrame,
    pub config: Option<CodecConfig>,
}

pub struct Encoder {
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    codec: CodecId,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    layout: PixelLayout,
    provides_samples: bool,
    output_bytes: u32,
    next_id: u64,
    parameter_sets: Vec<Vec<u8>>,
    _mf: MfGuard,
}

impl Encoder {
    pub fn open(
        codec: CodecId,
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
    ) -> Result<Self, PlatformError> {
        let width = even(width);
        let height = even(height);
        let fps = fps.clamp(1, 240);
        let bitrate = bitrate.max(1);
        let mf = MfGuard::startup()?;
        let activates = enumerate_mfts(MFT_CATEGORY_VIDEO_ENCODER, None, Some(video_subtype(codec)));
        let transform = activate_first(&activates)?;
        unlock_async(&transform);
        let codec_api = transform.cast::<ICodecAPI>().ok();
        if let Some(api) = &codec_api {
            set_codec_value(api, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            set_codec_value(
                api,
                &CODECAPI_AVEncCommonRateControlMode,
                VARIANT::from(eAVEncCommonRateControlMode_CBR.0),
            );
            set_codec_value(api, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
            set_codec_value(api, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(MAX_GOP));
            set_codec_value(api, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
        }
        let layout = negotiate(&transform, codec, width, height, fps, bitrate)?;
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(PlatformError::new)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(PlatformError::new)?;
        }
        let info = unsafe { transform.GetOutputStreamInfo(0) }.map_err(PlatformError::new)?;
        let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
        let output_bytes = info.cbSize.max(256 * 1024).min(8 * 1024 * 1024);
        Ok(Self {
            transform,
            codec_api,
            codec,
            width,
            height,
            fps,
            bitrate,
            layout,
            provides_samples,
            output_bytes,
            next_id: 1,
            parameter_sets: Vec::new(),
            _mf: mf,
        })
    }

    pub fn set_target_fps(&mut self, fps: u32) -> bool {
        let fps = fps.clamp(1, 60);
        if fps == self.fps {
            return true;
        }
        self.fps = fps;
        if let Ok(output) = unsafe { self.transform.GetOutputCurrentType(0) } {
            let _ = unsafe { output.SetUINT64(&MF_MT_FRAME_RATE, pack_size(fps, 1)) };
            let _ = unsafe { self.transform.SetOutputType(0, &output, 0) };
        }
        true
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> bool {
        let bitrate = bitrate.max(1);
        if bitrate == self.bitrate {
            return true;
        }
        let mut applied = false;
        if let Some(api) = &self.codec_api {
            applied = set_codec_value(api, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
        }
        if let Ok(output) = unsafe { self.transform.GetOutputCurrentType(0) } {
            if unsafe { output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate) }.is_ok()
                && unsafe { self.transform.SetOutputType(0, &output, 0) }.is_ok()
            {
                applied = true;
            }
        }
        if applied {
            self.bitrate = bitrate;
        }
        applied
    }

    pub fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        capture_us: u64,
        force_key: bool,
    ) -> Result<Option<Encoded>, PlatformError> {
        let width = even(width);
        let height = even(height);
        if width != self.width || height != self.height {
            return Err(PlatformError::new(format!(
                "frame is {width}x{height}, encoder is {}x{}",
                self.width, self.height
            )));
        }
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() < expected {
            return Err(PlatformError::new("frame buffer is shorter than its dimensions"));
        }
        if force_key {
            if let Some(api) = &self.codec_api {
                set_codec_value(api, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(true));
            }
        }
        let started = mono_us();
        let pixels = match self.layout {
            PixelLayout::Rgb32 => bgra[..expected].to_vec(),
            PixelLayout::Nv12 => bgra_to_nv12(&bgra[..expected], self.width, self.height),
        };
        let sample = make_sample(&pixels)?;
        let tick = 10_000_000i64 / i64::from(self.fps.max(1));
        let when = tick.saturating_mul(self.next_id.saturating_sub(1) as i64);
        unsafe {
            sample.SetSampleTime(when).ok();
            sample.SetSampleDuration(tick).ok();
            self.transform.ProcessInput(0, &sample, 0).map_err(PlatformError::new)?;
        }
        let Some((bytes, clean)) = self.drain_output()? else {
            return Ok(None);
        };
        let nals = payload_nals(self.codec, &bytes);
        if nals.is_empty() {
            return Ok(None);
        }
        let keyframe = force_key || clean || nals.iter().any(|nal| is_idr(self.codec, nal));
        let sets = self.read_parameter_sets(&nals);
        let config = if !sets.is_empty() && sets != self.parameter_sets {
            self.parameter_sets = sets.clone();
            Some(CodecConfig {
                codec: self.codec,
                width: self.width,
                height: self.height,
                nal_length_size: 4,
                parameter_sets: sets,
            })
        } else {
            None
        };
        let picture: Vec<Vec<u8>> = nals
            .iter()
            .filter(|nal| !is_parameter_nal(self.codec, nal))
            .cloned()
            .collect();
        let payload = if picture.is_empty() { nals_to_avcc(&nals) } else { nals_to_avcc(&picture) };
        if payload.is_empty() {
            return Ok(None);
        }
        let encode_us = u32::try_from(mono_us().saturating_sub(started)).unwrap_or(u32::MAX);
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        Ok(Some(Encoded {
            frame: VideoFrame {
                id,
                keyframe,
                codec: self.codec,
                capture_us,
                encode_us,
                payload,
            },
            config,
        }))
    }

    fn read_parameter_sets(&self, nals: &[Vec<u8>]) -> Vec<Vec<u8>> {
        if let Ok(output) = unsafe { self.transform.GetOutputCurrentType(0) } {
            if let Ok(size) = unsafe { output.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
                if (1..64 * 1024).contains(&size) {
                    let mut header = vec![0u8; size as usize];
                    if unsafe { output.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None) }.is_ok() {
                        let sets = parameter_sets_from_header(self.codec, &header);
                        if !sets.is_empty() {
                            return sets;
                        }
                    }
                }
            }
        }
        nals.iter()
            .filter(|nal| is_parameter_nal(self.codec, nal))
            .cloned()
            .collect()
    }

    fn drain_output(&mut self) -> Result<Option<(Vec<u8>, bool)>, PlatformError> {
        let mut produced = None;
        for _ in 0..8 {
            let mut buffer = MFT_OUTPUT_DATA_BUFFER::default();
            if !self.provides_samples {
                buffer.pSample = ManuallyDrop::new(Some(make_sample(&vec![0u8; self.output_bytes as usize])?));
            }
            let mut status = 0u32;
            let result = unsafe { self.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
            let sample = crate::codec::take_output_sample(&mut buffer.pSample);
            let _events = unsafe { ManuallyDrop::take(&mut buffer.pEvents) };
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        let clean = clean_point(&sample);
                        if let Ok(bytes) = crate::codec::copy_sample_bytes(&sample) {
                            if !bytes.is_empty() {
                                produced = Some((bytes, clean));
                            }
                        }
                    }
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if let Ok(next) = unsafe { self.transform.GetOutputAvailableType(0, 0) } {
                        unsafe { self.transform.SetOutputType(0, &next, 0).ok() };
                    }
                }
                Err(err) => return Err(PlatformError::new(err)),
            }
        }
        Ok(produced)
    }
}

fn clean_point(sample: &IMFSample) -> bool {
    unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.ok() == Some(1)
}

fn unlock_async(transform: &IMFTransform) {
    if let Ok(attrs) = unsafe { transform.GetAttributes() } {
        let _ = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
    }
}

fn set_codec_value(api: &ICodecAPI, key: &windows::core::GUID, value: VARIANT) -> bool {
    unsafe { api.SetValue(key, &value) }.is_ok()
}

fn negotiate(
    transform: &IMFTransform,
    codec: CodecId,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<PixelLayout, PlatformError> {
    let subtype = video_subtype(codec);
    let output = make_video_type(&subtype, width, height, fps, Some(bitrate))?;
    if unsafe { transform.SetOutputType(0, &output, 0) }.is_err() {
        let mut set = false;
        for index in 0..16u32 {
            let Ok(available) = (unsafe { transform.GetOutputAvailableType(0, index) }) else {
                break;
            };
            unsafe {
                available.SetGUID(&MF_MT_SUBTYPE, &subtype).ok();
                available.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height)).ok();
                available.SetUINT64(&MF_MT_FRAME_RATE, pack_size(fps, 1)).ok();
                available.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).ok();
                available
                    .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                    .ok();
            }
            if unsafe { transform.SetOutputType(0, &available, 0) }.is_ok() {
                set = true;
                break;
            }
        }
        if !set {
            return Err(PlatformError::new(format!(
                "hardware {} encoder rejected {}x{}",
                codec.name(),
                width,
                height
            )));
        }
    }
    for layout in [PixelLayout::Rgb32, PixelLayout::Nv12] {
        let input_kind = input_subtype(layout);
        let input = make_video_type(&input_kind, width, height, fps, None)?;
        if unsafe { transform.SetInputType(0, &input, 0) }.is_ok() {
            return Ok(layout);
        }
    }
    for index in 0..16u32 {
        let Ok(available) = (unsafe { transform.GetInputAvailableType(0, index) }) else {
            break;
        };
        let Ok(subtype) = (unsafe { available.GetGUID(&MF_MT_SUBTYPE) }) else {
            continue;
        };
        let layout = if subtype == input_subtype(PixelLayout::Rgb32) {
            PixelLayout::Rgb32
        } else if subtype == input_subtype(PixelLayout::Nv12) {
            PixelLayout::Nv12
        } else {
            continue;
        };
        unsafe {
            available.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height)).ok();
            available.SetUINT64(&MF_MT_FRAME_RATE, pack_size(fps, 1)).ok();
        }
        if unsafe { transform.SetInputType(0, &available, 0) }.is_ok() {
            return Ok(layout);
        }
    }
    Err(PlatformError::new("hardware encoder has no RGB32 or NV12 input"))
}
