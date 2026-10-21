//! A small first picture so a slow link has something to show quickly.
//!
//! The full frame follows on its own. Pointer coordinates stay in the capture's
//! pixel space; the viewer scales them to whichever picture it is showing.

use apple_cf::cv::CVPixelBuffer;
use apple_cf::iosurface::IOSurface;
use deskbridge_proto::CodecId;
use videotoolbox::transfer::{PixelTransferSession, ScalingMode};

use crate::capture::fit_capture_size;
use crate::encode::{EncodedPacket, Encoder};
use crate::error::PlatformError;

const PREVIEW_MAX_W: u32 = 640;
const PREVIEW_MAX_H: u32 = 360;
const PREVIEW_BITRATE: u32 = 1_000_000;
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

pub fn preview_size(width: u32, height: u32) -> (u32, u32) {
    fit_capture_size(width, height, PREVIEW_MAX_W, PREVIEW_MAX_H)
}

pub fn needs_preview(width: u32, height: u32) -> bool {
    let (preview_w, preview_h) = preview_size(width, height);
    preview_w < width || preview_h < height
}

pub struct Preview {
    encoder: Encoder,
    transfer: PixelTransferSession,
    surface: IOSurface,
    pub width: u32,
    pub height: u32,
}

impl Preview {
    pub fn open(codec: CodecId, width: u32, height: u32, fps: u32) -> Result<Self, PlatformError> {
        let (preview_w, preview_h) = preview_size(width, height);
        let encoder = Encoder::open(codec, preview_w, preview_h, PREVIEW_BITRATE, fps)?;
        let transfer = PixelTransferSession::new().map_err(PlatformError::new)?;
        let _ = transfer.set_real_time(true);
        let _ = transfer.set_scaling_mode(ScalingMode::Normal);
        let surface = IOSurface::create(preview_w as usize, preview_h as usize, BGRA, 4)
            .ok_or_else(|| PlatformError::new("preview surface"))?;
        Ok(Self {
            encoder,
            transfer,
            surface,
            width: preview_w,
            height: preview_h,
        })
    }

    pub fn encode(
        &mut self,
        source: &IOSurface,
        capture_us: u64,
    ) -> Result<Option<EncodedPacket>, PlatformError> {
        let src = CVPixelBuffer::create_with_io_surface(source)
            .map_err(|code| PlatformError::new(format!("preview source ({code})")))?;
        let dst = CVPixelBuffer::create_with_io_surface(&self.surface)
            .map_err(|code| PlatformError::new(format!("preview destination ({code})")))?;
        self.transfer
            .transfer(&src, &dst)
            .map_err(PlatformError::new)?;
        self.encoder.encode(&self.surface, capture_us, true)
    }
}

/// Scales a captured frame down to the size the viewer asked for.
pub struct FrameFit {
    transfer: PixelTransferSession,
    surface: IOSurface,
}

impl FrameFit {
    pub fn open(width: u32, height: u32) -> Result<Self, PlatformError> {
        let transfer = PixelTransferSession::new().map_err(PlatformError::new)?;
        let _ = transfer.set_real_time(true);
        let _ = transfer.set_scaling_mode(ScalingMode::Normal);
        let surface = IOSurface::create(width as usize, height as usize, BGRA, 4)
            .ok_or_else(|| PlatformError::new("scaled capture surface"))?;
        Ok(Self { transfer, surface })
    }

    pub fn apply<'a>(&'a mut self, source: &IOSurface) -> Result<&'a IOSurface, PlatformError> {
        let src = CVPixelBuffer::create_with_io_surface(source)
            .map_err(|code| PlatformError::new(format!("scale source ({code})")))?;
        let dst = CVPixelBuffer::create_with_io_surface(&self.surface)
            .map_err(|code| PlatformError::new(format!("scale destination ({code})")))?;
        self.transfer
            .transfer(&src, &dst)
            .map_err(PlatformError::new)?;
        Ok(&self.surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_of_a_large_frame_stays_small() {
        let codecs = crate::codec::hardware_encode_codecs();
        let codec = codecs.first().copied().expect("hardware encoder");
        let width = 960usize;
        let height = 540usize;
        let surface = IOSurface::create(width, height, BGRA, 4).expect("surface");
        {
            let mut guard = surface.lock_read_write().expect("lock");
            let dst = guard.base_address_mut().expect("base");
            let row = guard.bytes_per_row();
            for y in 0..height {
                for x in 0..width {
                    unsafe {
                        let pixel = dst.add(y * row + x * 4);
                        *pixel = 40;
                        *pixel.add(1) = 80;
                        *pixel.add(2) = 200;
                        *pixel.add(3) = 255;
                    }
                }
            }
        }
        let mut preview = Preview::open(codec, width as u32, height as u32, 60).expect("preview");
        assert_eq!((preview.width, preview.height), (640, 360));
        let packet = preview.encode(&surface, 1).expect("encode").expect("frame");
        let config = packet.config.expect("codec config");
        assert_eq!((config.width, config.height), (640, 360));
        assert!(packet.frame.keyframe);
        assert!(
            packet.frame.payload.len() < 200_000,
            "preview is {} bytes",
            packet.frame.payload.len()
        );
    }

    #[test]
    fn a_large_display_gets_a_640_by_360_preview() {
        assert_eq!(preview_size(5120, 2880), (640, 360));
        assert!(needs_preview(5120, 2880));
        assert_eq!(preview_size(1920, 1080), (640, 360));
        assert_eq!(preview_size(640, 360), (640, 360));
        assert!(!needs_preview(640, 360));
        assert!(!needs_preview(320, 180));
    }
}
