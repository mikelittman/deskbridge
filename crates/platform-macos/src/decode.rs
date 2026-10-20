use std::ptr;
use std::sync::{Arc, Mutex};

use apple_cf::cm::{CMBlockBuffer, CMFormatDescription, CMSampleBuffer};
use apple_cf::cv::CVPixelBuffer;
use apple_cf::iosurface::IOSurface;
use apple_cf::raw::{
    self, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, CMSampleBufferCreateReady,
};
use deskbridge_proto::{CodecConfig, CodecId, VideoFrame};
use videotoolbox::transfer::PixelTransferSession;
use videotoolbox::{DecompressionSession, DecodedFrame};

use crate::codec::vt_codec;
use crate::error::PlatformError;

const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

struct Slot {
    frame: Mutex<Option<CVPixelBuffer>>,
}

pub struct Decoder {
    codec: Option<CodecId>,
    parameter_sets: Vec<Vec<u8>>,
    session: Option<DecompressionSession>,
    slot: Arc<Slot>,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            codec: None,
            parameter_sets: Vec::new(),
            session: None,
            slot: Arc::new(Slot {
                frame: Mutex::new(None),
            }),
        }
    }

    pub fn decode(
        &mut self,
        config: &CodecConfig,
        frame: &VideoFrame,
    ) -> Result<Option<IOSurface>, PlatformError> {
        if frame.payload.is_empty() {
            return Ok(None);
        }
        if self.codec != Some(config.codec) || self.parameter_sets != config.parameter_sets {
            self.session = Some(open_decoder(config, Arc::clone(&self.slot))?);
            self.codec = Some(config.codec);
            self.parameter_sets = config.parameter_sets.clone();
        }
        let sample = sample_buffer(config, &frame.payload)?;
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| PlatformError::new("decoder is not open"))?;
        session.decode(&sample).map_err(PlatformError::new)?;
        let _ = session.wait_for_async_frames();
        let buffer = self
            .slot
            .frame
            .lock()
            .expect("decoder slot")
            .take();
        let Some(buffer) = buffer else {
            return Ok(None);
        };
        if let Some(surface) = buffer.io_surface() {
            return Ok(Some(surface));
        }
        copy_to_bgra(&buffer).map(Some)
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

fn open_decoder(config: &CodecConfig, slot: Arc<Slot>) -> Result<DecompressionSession, PlatformError> {
    if !DecompressionSession::is_hardware_decode_supported(vt_codec(config.codec)) {
        return Err(PlatformError::new(format!(
            "this Mac has no hardware {} decoder",
            config.codec.name()
        )));
    }
    let format = format_description(config)?;
    if let Some(attributes) = destination_attributes() {
        let slot_for_try = Arc::clone(&slot);
        let opened = DecompressionSession::new_with_image_buffer_attributes(
            &format,
            Some(&attributes),
            move |decoded: DecodedFrame| store_decoded(&slot_for_try, decoded),
        );
        if let Ok(session) = opened {
            return Ok(session);
        }
    }
    DecompressionSession::new(&format, move |decoded: DecodedFrame| store_decoded(&slot, decoded))
        .map_err(PlatformError::new)
}

fn store_decoded(slot: &Slot, decoded: DecodedFrame) {
    if decoded.status != 0 {
        return;
    }
    if let Some(buffer) = decoded.image_buffer {
        *slot.frame.lock().expect("decoder slot") = Some(buffer);
    }
}

fn destination_attributes() -> Option<apple_cf::cf::CFDictionary> {
    let format_key = unsafe {
        apple_cf::cf::CFString::from_raw_borrowed(
            raw::kCVPixelBufferPixelFormatTypeKey.cast_mut().cast(),
        )
    }?;
    let surface_key = unsafe {
        apple_cf::cf::CFString::from_raw_borrowed(
            raw::kCVPixelBufferIOSurfacePropertiesKey.cast_mut().cast(),
        )
    }?;
    let format = apple_cf::cf::CFNumber::from_i64(i64::from(BGRA));
    let empty = apple_cf::cf::CFDictionary::from_pairs(&[]);
    Some(apple_cf::cf::CFDictionary::from_pairs(&[
        (&format_key, &format),
        (&surface_key, &empty),
    ]))
}

fn format_description(config: &CodecConfig) -> Result<CMFormatDescription, PlatformError> {
    if config.parameter_sets.is_empty() {
        return Err(PlatformError::new("codec config has no parameter sets"));
    }
    let pointers: Vec<*const u8> = config.parameter_sets.iter().map(|set| set.as_ptr()).collect();
    let sizes: Vec<usize> = config.parameter_sets.iter().map(|set| set.len()).collect();
    let mut out: raw::CMFormatDescriptionRef = ptr::null();
    let nal = i32::from(config.nal_length_size.max(1));
    let status = unsafe {
        match config.codec {
            CodecId::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                raw::kCFAllocatorDefault,
                pointers.len(),
                pointers.as_ptr(),
                sizes.as_ptr(),
                nal,
                ptr::null(),
                &raw mut out,
            ),
            CodecId::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                raw::kCFAllocatorDefault,
                pointers.len(),
                pointers.as_ptr(),
                sizes.as_ptr(),
                nal,
                &raw mut out,
            ),
        }
    };
    if status != 0 {
        return Err(PlatformError::new(format!(
            "format description was rejected ({status})"
        )));
    }
    unsafe { CMFormatDescription::from_raw(out.cast_mut().cast()) }
        .ok_or_else(|| PlatformError::new("format description was empty"))
}

fn sample_buffer(config: &CodecConfig, payload: &[u8]) -> Result<CMSampleBuffer, PlatformError> {
    let format = format_description(config)?;
    let block = CMBlockBuffer::create(payload)
        .ok_or_else(|| PlatformError::new("could not copy the frame into a block buffer"))?;
    let time = |value: i64| raw::CMTime {
        value,
        timescale: 1_000_000,
        flags: 1,
        epoch: 0,
    };
    let timing = raw::CMSampleTimingInfo {
        duration: time(1),
        presentationTimeStamp: time(0),
        decodeTimeStamp: time(0),
    };
    let size = payload.len();
    let mut sample: raw::CMSampleBufferRef = ptr::null_mut();
    let status = unsafe {
        CMSampleBufferCreateReady(
            raw::kCFAllocatorDefault,
            block.as_ptr().cast(),
            format.as_ptr().cast(),
            1,
            1,
            &raw const timing,
            1,
            &raw const size,
            &raw mut sample,
        )
    };
    if status != 0 {
        return Err(PlatformError::new(format!(
            "sample buffer was rejected ({status})"
        )));
    }
    unsafe { CMSampleBuffer::from_raw(sample.cast()) }
        .ok_or_else(|| PlatformError::new("sample buffer was empty"))
}

pub fn copy_to_bgra(buffer: &CVPixelBuffer) -> Result<IOSurface, PlatformError> {
    if buffer.pixel_format() == BGRA {
        if let Some(surface) = buffer.io_surface() {
            return Ok(surface);
        }
    }
    let surface = IOSurface::create(buffer.width(), buffer.height(), BGRA, 4)
        .ok_or_else(|| PlatformError::new("could not allocate a BGRA surface"))?;
    let destination = CVPixelBuffer::create_with_io_surface(&surface).map_err(PlatformError::new)?;
    let transfer = PixelTransferSession::new().map_err(PlatformError::new)?;
    transfer
        .transfer(buffer, &destination)
        .map_err(PlatformError::new)?;
    Ok(surface)
}

/// Read one BGRA pixel. Returns `None` when the surface is not BGRA.
#[cfg(test)]
pub fn sample_bgra(surface: &IOSurface, x: usize, y: usize) -> Option<[u8; 4]> {
    if surface.pixel_format() != BGRA || x >= surface.width() || y >= surface.height() {
        return None;
    }
    let guard = surface.lock_read_only().ok()?;
    let row = guard.bytes_per_row();
    let base = guard.base_address();
    if base.is_null() {
        return None;
    }
    let offset = y * row + x * 4;
    let pixel = unsafe { std::ptr::read_unaligned(base.add(offset) as *const [u8; 4]) };
    Some(pixel)
}
