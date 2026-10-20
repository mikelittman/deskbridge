use apple_cf::cm::CMTime;
use apple_cf::iosurface::IOSurface;
use deskbridge_proto::{CodecConfig, CodecId, VideoFrame};
use deskbridge_session::mono_us;
use videotoolbox::compression::{
    CompressionSession, FrameProperties, HardwareAcceleration,
};
use videotoolbox::ffi::{
    kCFAllocatorDefault, kCFNumberFloat64Type, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, CFNumberCreate, CFRelease,
};
use videotoolbox::VTError;

use crate::codec::vt_codec;
use crate::error::PlatformError;
use crate::pace::encode_fps;

const MAX_KEYFRAME_INTERVAL: i32 = 3600;

pub struct EncodedPacket {
    pub frame: VideoFrame,
    pub config: Option<CodecConfig>,
}

pub struct Encoder {
    session: CompressionSession,
    codec: CodecId,
    width: u32,
    height: u32,
    /// Timescale for frame timestamps. Stays at the capture rate.
    fps: u32,
    target_fps: u32,
    next_id: u64,
    parameter_sets: Vec<Vec<u8>>,
    nal_length_size: u8,
}

impl Encoder {
    pub fn open(
        codec: CodecId,
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
    ) -> Result<Self, PlatformError> {
        let fps = fps.clamp(1, 240);
        let session = open_session(codec, width, height, bitrate, fps)?;
        confirm_hardware(&session)?;
        Ok(Self {
            session,
            codec,
            width,
            height,
            fps,
            target_fps: encode_fps(fps, None),
            next_id: 1,
            parameter_sets: Vec::new(),
            nal_length_size: 4,
        })
    }

    /// Tell the encoder how many frames per second we intend to send.
    /// Bits per frame follow this rate. An unchanged value does nothing.
    pub fn set_target_fps(&mut self, fps: u32) -> bool {
        let fps = fps.clamp(1, 60);
        if fps == self.target_fps {
            return true;
        }
        let value = f64::from(fps);
        let number = unsafe {
            CFNumberCreate(
                kCFAllocatorDefault,
                kCFNumberFloat64Type,
                std::ptr::from_ref(&value).cast(),
            )
        };
        if number.is_null() {
            return false;
        }
        let result = unsafe {
            self.session
                .set_property(kVTCompressionPropertyKey_ExpectedFrameRate, number.cast())
        };
        unsafe { CFRelease(number.cast()) };
        if result.is_ok() {
            self.target_fps = fps;
            true
        } else {
            false
        }
    }

    pub fn encode(
        &mut self,
        surface: &IOSurface,
        capture_us: u64,
        force_key: bool,
    ) -> Result<Option<EncodedPacket>, PlatformError> {
        let started = mono_us();
        let time = CMTime::new(self.next_id as i64, self.fps as i32);
        let properties = FrameProperties::new().with_force_key_frame(force_key);
        let encoded = self
            .session
            .encode_with_properties(surface, time, properties)
            .map_err(PlatformError::new)?;
        if encoded.data.is_empty() {
            return Ok(None);
        }
        let sample = encoded
            .cm_sample_buffer()
            .ok_or_else(|| PlatformError::new("encoder returned no sample buffer"))?;
        let keyframe = sample.is_sync_sample();
        let format = sample
            .format_description()
            .ok_or_else(|| PlatformError::new("encoded frame has no format description"))?;
        let sets = format
            .video_parameter_sets()
            .map_err(|status| PlatformError::new(format!("parameter sets unavailable ({status})")))?;
        let config = if sets.parameter_sets != self.parameter_sets {
            self.parameter_sets = sets.parameter_sets.clone();
            self.nal_length_size = u8::try_from(sets.nal_unit_header_length).unwrap_or(4);
            Some(CodecConfig {
                codec: self.codec,
                width: self.width,
                height: self.height,
                nal_length_size: self.nal_length_size,
                parameter_sets: sets.parameter_sets,
            })
        } else {
            None
        };
        let encode_us = u32::try_from(mono_us().saturating_sub(started)).unwrap_or(u32::MAX);
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        Ok(Some(EncodedPacket {
            frame: VideoFrame {
                id,
                keyframe,
                codec: self.codec,
                capture_us,
                encode_us,
                payload: encoded.data,
            },
            config,
        }))
    }
}

fn open_session(
    codec: CodecId,
    width: u32,
    height: u32,
    bitrate: u32,
    fps: u32,
) -> Result<CompressionSession, PlatformError> {
    let bitrate = i32::try_from(bitrate).unwrap_or(i32::MAX);
    let mut last = PlatformError::new("hardware encoder did not start");
    for low_latency in [true, false] {
        let mut builder = CompressionSession::builder(width as i32, height as i32, vt_codec(codec))
            .with_real_time(true)
            .with_allow_frame_reordering(false)
            .with_hardware_acceleration(HardwareAcceleration::Required)
            .with_average_bit_rate(bitrate)
            .with_expected_frame_rate(f64::from(encode_fps(fps, None)))
            .with_max_keyframe_interval(MAX_KEYFRAME_INTERVAL);
        if low_latency {
            builder = builder.with_low_latency_rate_control(true);
        }
        match builder.build() {
            Ok(session) => return Ok(session),
            Err(err) => last = PlatformError::new(err),
        }
    }
    Err(last)
}

fn confirm_hardware(session: &CompressionSession) -> Result<(), PlatformError> {
    // The session was opened with hardware required, so creation already fails
    // when only a software encoder is available. Some Macs return -12900
    // (property not supported) for this read-only flag; that is not a software encoder.
    let value = unsafe {
        session.copy_property(kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder)
    };
    let value = match value {
        Ok(value) => value,
        Err(VTError::ApiFailed { status: -12900, .. }) => return Ok(()),
        Err(err) => return Err(PlatformError::new(err)),
    };
    let Some(value) = value else {
        return Err(PlatformError::new(
            "VideoToolbox did not report a hardware encoder",
        ));
    };
    let hardware = unsafe { apple_cf::raw::CFBooleanGetValue(value.as_ptr().cast()) } != 0;
    if hardware {
        Ok(())
    } else {
        Err(PlatformError::new(
            "VideoToolbox selected a software encoder; Deskbridge stays on the hardware path",
        ))
    }
}
